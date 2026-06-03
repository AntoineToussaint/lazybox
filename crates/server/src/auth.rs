use crate::ServerConfig;
use chrono::{DateTime, Utc};
use lazybox_auth::Credential;
use lazybox_ipc::{Event, PrincipalId, ProviderCredentialInput, ProviderCredentialMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{Mutex, mpsc};

pub type CredentialStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, CredentialStoreError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentialMetadata {
    pub principal_id: String,
    pub provider_id: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct ProviderCredential {
    pub credential: Credential,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl ProviderCredential {
    pub fn new(
        credential: Credential,
        scopes: Vec<String>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            credential,
            scopes,
            expires_at,
        }
    }
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredential")
            .field("credential", &self.credential)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    #[error("credential not found for principal `{principal_id}` provider `{provider_id}`")]
    NotFound {
        principal_id: String,
        provider_id: String,
    },
    #[error("credential store failed: {0}")]
    Provider(String),
}

pub trait CredentialStore: Send + Sync {
    fn put_provider_credential<'a>(
        &'a self,
        principal_id: &'a str,
        provider_id: &'a str,
        credential: ProviderCredential,
    ) -> CredentialStoreFuture<'a, StoredCredentialMetadata>;

    fn get_provider_credential<'a>(
        &'a self,
        principal_id: &'a str,
        provider_id: &'a str,
    ) -> CredentialStoreFuture<'a, Credential>;

    fn delete_provider_credential<'a>(
        &'a self,
        principal_id: &'a str,
        provider_id: &'a str,
    ) -> CredentialStoreFuture<'a, ()>;

    fn list_provider_credentials<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> CredentialStoreFuture<'a, Vec<StoredCredentialMetadata>>;
}

#[derive(Clone)]
struct StoredCredential {
    credential: Credential,
    metadata: StoredCredentialMetadata,
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredential")
            .field("credential", &self.credential)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[derive(Default)]
pub struct MemoryCredentialStore {
    inner: Mutex<HashMap<(String, String), StoredCredential>>,
}

impl MemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl fmt::Debug for MemoryCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryCredentialStore")
            .finish_non_exhaustive()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn put_provider_credential<'a>(
        &'a self,
        principal_id: &'a str,
        provider_id: &'a str,
        credential: ProviderCredential,
    ) -> CredentialStoreFuture<'a, StoredCredentialMetadata> {
        Box::pin(async move {
            let metadata = StoredCredentialMetadata {
                principal_id: principal_id.to_string(),
                provider_id: provider_id.to_string(),
                source: credential.credential.source.clone(),
                scopes: credential.scopes.clone(),
                updated_at: Utc::now(),
                expires_at: credential.expires_at,
            };
            let stored = StoredCredential {
                credential: credential.credential,
                metadata: metadata.clone(),
            };
            self.inner
                .lock()
                .await
                .insert((principal_id.to_string(), provider_id.to_string()), stored);
            Ok(metadata)
        })
    }

    fn get_provider_credential<'a>(
        &'a self,
        principal_id: &'a str,
        provider_id: &'a str,
    ) -> CredentialStoreFuture<'a, Credential> {
        Box::pin(async move {
            self.inner
                .lock()
                .await
                .get(&(principal_id.to_string(), provider_id.to_string()))
                .map(|stored| stored.credential.clone())
                .ok_or_else(|| CredentialStoreError::NotFound {
                    principal_id: principal_id.to_string(),
                    provider_id: provider_id.to_string(),
                })
        })
    }

    fn delete_provider_credential<'a>(
        &'a self,
        principal_id: &'a str,
        provider_id: &'a str,
    ) -> CredentialStoreFuture<'a, ()> {
        Box::pin(async move {
            self.inner
                .lock()
                .await
                .remove(&(principal_id.to_string(), provider_id.to_string()));
            Ok(())
        })
    }

    fn list_provider_credentials<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> CredentialStoreFuture<'a, Vec<StoredCredentialMetadata>> {
        Box::pin(async move {
            let mut credentials: Vec<_> = self
                .inner
                .lock()
                .await
                .values()
                .filter(|stored| stored.metadata.principal_id == principal_id)
                .map(|stored| stored.metadata.clone())
                .collect();
            credentials.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
            Ok(credentials)
        })
    }
}

pub async fn handle_upsert_provider_credential(
    config: &ServerConfig,
    tx: &mpsc::UnboundedSender<Event>,
    principal_id: PrincipalId,
    input: ProviderCredentialInput,
) {
    let provider_id = input.provider_id.clone();
    let credential = ProviderCredential::new(
        Credential::new(input.token, input.source),
        input.scopes,
        input.expires_at,
    );
    match config
        .credential_store
        .put_provider_credential(principal_id.as_str(), &provider_id, credential)
        .await
    {
        Ok(metadata) => {
            let _ = tx.send(Event::ProviderCredentialUpdated {
                principal_id,
                provider_id,
                metadata: into_core_metadata(metadata),
            });
        }
        Err(error) => {
            send_auth_error(tx, &provider_id, error);
        }
    }
}

pub async fn handle_remove_provider_credential(
    config: &ServerConfig,
    tx: &mpsc::UnboundedSender<Event>,
    principal_id: PrincipalId,
    provider_id: String,
) {
    match config
        .credential_store
        .delete_provider_credential(principal_id.as_str(), &provider_id)
        .await
    {
        Ok(()) => {
            let _ = tx.send(Event::ProviderCredentialRemoved {
                principal_id,
                provider_id,
            });
        }
        Err(error) => {
            send_auth_error(tx, &provider_id, error);
        }
    }
}

pub async fn handle_list_provider_credentials(
    config: &ServerConfig,
    tx: &mpsc::UnboundedSender<Event>,
    principal_id: PrincipalId,
) {
    match config
        .credential_store
        .list_provider_credentials(principal_id.as_str())
        .await
    {
        Ok(credentials) => {
            let _ = tx.send(Event::ProviderCredentialsListed {
                principal_id,
                credentials: credentials.into_iter().map(into_core_metadata).collect(),
            });
        }
        Err(error) => {
            send_auth_error(tx, "auth", error);
        }
    }
}

fn into_core_metadata(metadata: StoredCredentialMetadata) -> ProviderCredentialMetadata {
    ProviderCredentialMetadata {
        principal_id: PrincipalId::new(metadata.principal_id),
        provider_id: metadata.provider_id,
        source: metadata.source,
        scopes: metadata.scopes,
        updated_at: metadata.updated_at,
        expires_at: metadata.expires_at,
    }
}

fn send_auth_error(
    tx: &mpsc::UnboundedSender<Event>,
    provider_id: &str,
    error: CredentialStoreError,
) {
    let _ = tx.send(Event::provider_error(
        &format!("auth:{provider_id}"),
        error.to_string(),
        lazybox_ipc::ProviderErrorKind::Auth,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_put_get_list_delete() {
        let store = MemoryCredentialStore::new();
        let metadata = store
            .put_provider_credential(
                "local",
                "github",
                ProviderCredential::new(
                    Credential::new("ghp_secret", "unit-test"),
                    vec!["repo".into()],
                    None,
                ),
            )
            .await
            .unwrap();

        assert_eq!(metadata.principal_id, "local");
        assert_eq!(metadata.provider_id, "github");
        assert_eq!(metadata.source, "unit-test");
        assert_eq!(metadata.scopes, vec!["repo"]);

        let credential = store
            .get_provider_credential("local", "github")
            .await
            .unwrap();
        assert_eq!(credential.token(), "ghp_secret");

        let listed = store.list_provider_credentials("local").await.unwrap();
        assert_eq!(listed, vec![metadata]);

        store
            .delete_provider_credential("local", "github")
            .await
            .unwrap();
        let result = store.get_provider_credential("local", "github").await;
        assert!(matches!(result, Err(CredentialStoreError::NotFound { .. })));
    }

    #[test]
    fn debug_redacts_secret_material() {
        let credential = ProviderCredential::new(
            Credential::new("top-secret-token", "unit-test"),
            vec![],
            None,
        );

        let debug = format!("{credential:?}");

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("top-secret-token"));
    }
}
