//! The per-session token that marks a condensation as lazybox's own
//! (#1609, #1611).
//!
//! [`CondenseTag`]'s token is embedded in the rendered bytes of every
//! condensed block, and a block is re-rendered from the agent's original
//! transcript on every turn. So the token *is* the byte-stability
//! guarantee: condensing block *b* under a different token next turn
//! rewrites bytes the upstream was serving from its prompt cache, which is
//! the one cost this feature cannot afford. A token drawn per process
//! breaks exactly that on every daemon restart — mid-conversation, for
//! every block already condensed.
//!
//! It also has to be the *same* token wherever recognition happens. Two
//! enforcement points condense into one conversation: the proxy rewrites
//! the request body (#1609) and Claude's `PreToolUse` hook condenses
//! before the read lands in the transcript (#1610). A block the hook
//! condensed arrives in the proxy's next request body, and under a
//! different token the proxy would not recognize it as ours and would
//! condense the summary again.
//!
//! So the token is derived, not drawn: one random secret per installation,
//! persisted in the store, and a token per session derived from it.
//! Derived means no per-session write on the request path and no state to
//! lose across a restart; one-way means the token that rides upstream
//! inside the marker — where the model can read it, and where content the
//! agent reads must not be able to forge it — reveals nothing about the
//! secret or about any other session's token.
//!
//! **The secret is forgery-grade key material.** Session keys are public
//! (`github:owner/repo#42` is on screen), so whoever holds the secret can
//! compute every session's token, past and future, and mint text carrying
//! lazybox's provenance marker — the trust boundary #1611 built. That is a
//! wider blast radius than the per-session random token it replaces, which
//! compromised only its own session. It lives in `state.db` (0600), beside
//! the agent transcripts that already carry live tokens, rather than in the
//! OS keystore `crates/identity` uses for device keys: the marker
//! authenticates nothing outside this process, so the file mode is the
//! protection that matters. There is deliberately **no rotation path** —
//! rotating re-renders every condensed block at once, which needs the
//! cache-cost accounting #1621 owns.

use lazybox_core::CondenseTag;
use sha2::{Digest, Sha256};

/// kv key holding the installation's condensation secret.
const SECRET_KV_KEY: &str = "context-hygiene:secret";

/// Derives each session's [`CondenseTag`] from one persisted secret.
#[derive(Debug, Clone)]
pub struct TagSource {
    secret: String,
}

impl TagSource {
    /// Load the installation secret, seeding one the first time.
    ///
    /// Callers converge without coordinating: the seed is a conditional
    /// insert, so concurrent loads — two enforcement points in one daemon,
    /// or two daemons on one `state.db` — all read back the single value
    /// that won, rather than each keeping the one it proposed. Touching the
    /// store is confined to here; deriving a token afterwards is pure
    /// computation on the request path.
    pub async fn load(config: &crate::ServerConfig) -> Self {
        let candidate = uuid::Uuid::new_v4().simple().to_string();
        let proposed = candidate.clone();
        let seeded = crate::store_blocking(&config.store, move |store| {
            store.set_kv_if_absent(SECRET_KV_KEY, &proposed)
        })
        .await;

        match seeded {
            Ok(secret) if !secret.trim().is_empty() => Self::from_secret(secret.trim()),
            Ok(_) => {
                // Nothing here writes an empty secret, so the row was
                // corrupted from outside. Repair it: deriving every token
                // from an empty string would share them across
                // installations, and running ephemeral would re-render every
                // condensed block on every start for as long as the row sat
                // there.
                let repair = candidate.clone();
                if let Err(error) = crate::store_blocking(&config.store, move |store| {
                    store.set_kv(SECRET_KV_KEY, &repair)
                })
                .await
                {
                    tracing::warn!(
                        "context hygiene: the stored condensation secret is empty and \
                         replacing it failed ({error}); using an ephemeral one"
                    );
                }
                Self::from_secret(candidate)
            }
            Err(error) => {
                // A read that failed is not a key that is absent. Seeding
                // over a secret we merely could not read would replace it
                // for good — the kv write upserts — and every block
                // condensed under the old one re-renders on its next turn,
                // for every session, permanently. That is the cache collapse
                // this module exists to prevent, so an unreadable store buys
                // an ephemeral secret and a warning, never a write.
                tracing::warn!(
                    "context hygiene: reading the condensation secret failed ({error}); \
                     using an ephemeral one, so blocks condensed by this daemon will \
                     re-render after a restart"
                );
                Self::from_secret(candidate)
            }
        }
    }

    pub fn from_secret(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
        }
    }

    /// This session's tag. Stable for a given (secret, session), and
    /// one-way, so an observed token cannot be walked back to the secret
    /// or across to another session's token.
    pub fn tag(&self, session: &str) -> CondenseTag {
        let mut hasher = Sha256::new();
        // Length-prefixed so no two (secret, session) pairs share a
        // preimage — an installation secret ending in the first character
        // of a session key must not derive another installation's token.
        for field in [self.secret.as_str(), session] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        let digest = hasher.finalize();
        let mut token = String::with_capacity(digest.len() * 2);
        for byte in digest.iter() {
            use std::fmt::Write as _;
            let _ = write!(token, "{byte:02x}");
        }
        CondenseTag::new(&token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_store::Store as _;

    #[test]
    fn a_session_tag_is_stable_and_distinct_per_session() {
        let source = TagSource::from_secret("installation-secret");
        let one = source.tag("github-acme-widget-7");
        assert_eq!(
            one.prefix(),
            source.tag("github-acme-widget-7").prefix(),
            "the same session derives the same tag every time"
        );
        assert_ne!(
            one.prefix(),
            source.tag("github-acme-widget-8").prefix(),
            "a different session derives a different tag"
        );
    }

    #[test]
    fn the_token_does_not_leak_the_secret_or_other_sessions() {
        let source = TagSource::from_secret("installation-secret");
        let tag = source.tag("ws");
        assert!(
            !tag.prefix().contains("installation-secret"),
            "the marker that rides upstream must not carry the secret"
        );
        // A different installation derives different tokens from the same
        // session key, so a token observed in one transcript marks nothing
        // anywhere else.
        assert_ne!(
            tag.prefix(),
            TagSource::from_secret("another-secret").tag("ws").prefix()
        );
    }

    /// Length-prefixing is what makes this hold: without it the pair
    /// ("secret-a", "b") and ("secret-", "ab") hash the same bytes.
    #[test]
    fn a_field_boundary_shift_derives_a_different_tag() {
        assert_ne!(
            TagSource::from_secret("secret-a").tag("b").prefix(),
            TagSource::from_secret("secret-").tag("ab").prefix()
        );
    }

    /// The restart case: the secret is persisted, so a fresh daemon over
    /// the same store derives the same tags — and re-renders a block it
    /// condensed before the restart to the same bytes.
    #[tokio::test]
    async fn the_secret_survives_a_restart() {
        let config = crate::ServerConfig::in_memory();
        let first = TagSource::load(&config).await;
        let second = TagSource::load(&config).await;
        assert_eq!(
            first.tag("ws").prefix(),
            second.tag("ws").prefix(),
            "a restart over the same store keeps condensed bytes stable"
        );
    }

    /// Durability is the whole point, and it runs through SQLite in
    /// production — an in-memory map round-tripping proves the derivation,
    /// not the storage.
    #[tokio::test]
    async fn the_secret_round_trips_through_sqlite_across_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("state.db");
        let load_once = |store: std::sync::Arc<dyn lazybox_store::Store>| async move {
            TagSource::load(&crate::ServerConfig::with_store(store)).await
        };

        let first = load_once(std::sync::Arc::new(
            lazybox_store::SqliteStore::open(&db).expect("open"),
        ))
        .await;
        let second = load_once(std::sync::Arc::new(
            lazybox_store::SqliteStore::open(&db).expect("reopen"),
        ))
        .await;

        assert_eq!(
            first.tag("ws").prefix(),
            second.tag("ws").prefix(),
            "a real daemon restart over the same file keeps condensed bytes stable"
        );
    }

    /// Two enforcement points in one daemon — or two daemons on one file —
    /// must not each keep the secret they proposed. Seeding is a
    /// conditional insert precisely so the losers adopt the winner.
    #[tokio::test]
    async fn concurrent_loads_converge_on_one_secret() {
        let config = crate::ServerConfig::in_memory();
        let (first, second) = tokio::join!(TagSource::load(&config), TagSource::load(&config));
        assert_eq!(
            first.tag("ws").prefix(),
            second.tag("ws").prefix(),
            "concurrent loads must agree on the secret that won the seed"
        );
    }

    /// A store holding a real secret whose *reads* fail — the shape a
    /// `SQLITE_BUSY` from a second process on the same file takes. Both
    /// read paths fail (the plain get and the conditional seed) while
    /// writes land on the inner store, so a caller that reacts to a failed
    /// read by writing does visible damage rather than a silent no-op.
    #[derive(Default)]
    struct UnreadableStore {
        inner: lazybox_store::MemoryStore,
    }

    impl lazybox_store::Store for UnreadableStore {
        fn set_kv_if_absent(
            &self,
            _key: &str,
            _value: &str,
        ) -> Result<String, lazybox_store::StoreError> {
            Err(lazybox_store::StoreError::Backend(
                "database is locked".to_string(),
            ))
        }

        fn get_kv(&self, _key: &str) -> Result<Option<String>, lazybox_store::StoreError> {
            Err(lazybox_store::StoreError::Backend(
                "database is locked".to_string(),
            ))
        }

        fn set_kv(&self, key: &str, value: &str) -> Result<(), lazybox_store::StoreError> {
            self.inner.set_kv(key, value)
        }
    }

    /// The sharing #1645 needs is not the persistence. A daemon whose store
    /// cannot be read runs on an *ephemeral* secret, so two loads of it would
    /// mint different tokens for one session — and the proxy would stop
    /// recognizing what the hook condensed, the failure the shipped default's
    /// line floor merely hid. Both enforcement points therefore reach the one
    /// source the config holds rather than loading their own.
    #[tokio::test]
    async fn every_caller_shares_one_source_even_on_an_ephemeral_secret() {
        let config =
            crate::ServerConfig::with_store(std::sync::Arc::new(UnreadableStore::default()));
        let also = config.clone();
        let (proxy, hook) = tokio::join!(config.condense_tags(), also.condense_tags());
        assert_eq!(
            proxy.tag("ws").prefix(),
            hook.tag("ws").prefix(),
            "the hook and the proxy must condense under one token"
        );
    }

    /// The destructive case. A failed read is not an absent key: writing a
    /// fresh secret here would upsert over one that is merely unreadable,
    /// and every block condensed under it re-renders from then on, for
    /// every session, permanently.
    #[tokio::test]
    async fn a_failed_seed_never_replaces_the_secret_it_could_not_read() {
        let store = std::sync::Arc::new(UnreadableStore::default());
        store
            .inner
            .set_kv(SECRET_KV_KEY, "the-installation-secret")
            .expect("seed the existing secret");
        let config = crate::ServerConfig::with_store(
            store.clone() as std::sync::Arc<dyn lazybox_store::Store>
        );

        let source = TagSource::load(&config).await;

        assert_eq!(
            store.inner.get_kv(SECRET_KV_KEY).expect("read back"),
            Some("the-installation-secret".to_string()),
            "the stored secret must survive a store that could not be seeded"
        );
        assert_ne!(
            source.tag("ws").prefix(),
            TagSource::from_secret("the-installation-secret")
                .tag("ws")
                .prefix(),
            "and this daemon runs ephemeral rather than claiming it read that secret"
        );
    }
}
