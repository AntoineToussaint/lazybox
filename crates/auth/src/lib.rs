//! # lazybox-auth
//!
//! Modular credential resolution for lazybox.
//!
//! Provides the [`CredentialProvider`] trait and a [`CredentialChain`] that
//! tries multiple providers in order (like AWS credential chain).
//!
//! Built-in providers: environment variables, shell commands (e.g. `gh auth token`),
//! static tokens. Consumers can implement the trait for Vault, Keychain, OAuth, etc.

mod chain;
mod credential;
mod providers;

pub use chain::{CredentialChain, invalidate_chain_cache};
pub use credential::{Credential, CredentialError, CredentialProvider};
pub use providers::*;
