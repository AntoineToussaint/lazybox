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

use lazybox_core::CondenseTag;
use sha2::{Digest, Sha256};

/// kv key holding the installation's condensation secret.
const SECRET_KV_KEY: &str = "context-hygiene:secret";

/// Digest bytes kept as the token. 128 bits is far past guessing and short
/// enough that the marker stays readable in a transcript.
const TOKEN_BYTES: usize = 16;

/// Derives each session's [`CondenseTag`] from one persisted secret.
#[derive(Debug, Clone)]
pub struct TagSource {
    secret: String,
}

impl TagSource {
    /// Load the installation secret, generating and persisting one the
    /// first time.
    ///
    /// Call this **once per daemon** and hand the result to every
    /// enforcement point: two concurrent loads on a store with no secret
    /// yet would each generate one and race to persist it, leaving the two
    /// callers deriving different tokens for the same session. It touches
    /// the store, and the derived tokens are then pure computation on the
    /// request path.
    pub async fn load(config: &crate::ServerConfig) -> Self {
        if let Ok(Some(secret)) =
            crate::store_blocking(&config.store, |store| store.get_kv(SECRET_KV_KEY)).await
            && !secret.trim().is_empty()
        {
            return Self::from_secret(secret.trim());
        }
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let persisted = secret.clone();
        if let Err(error) = crate::store_blocking(&config.store, move |store| {
            store.set_kv(SECRET_KV_KEY, &persisted)
        })
        .await
        {
            // Not fatal — the secret works for this process. What is lost
            // is recognition of, and byte-stability for, everything
            // condensed before the next restart, so say so.
            tracing::warn!(
                "context hygiene: persisting the condensation secret failed ({error}); \
                 blocks condensed by this daemon will re-render differently after a restart"
            );
        }
        Self::from_secret(secret)
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
        let mut token = String::with_capacity(TOKEN_BYTES * 2);
        for byte in &digest[..TOKEN_BYTES] {
            use std::fmt::Write as _;
            let _ = write!(token, "{byte:02x}");
        }
        CondenseTag::new(&token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
