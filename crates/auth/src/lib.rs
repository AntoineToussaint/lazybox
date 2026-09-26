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

pub use chain::{CredentialChain, forget_failed_chain_resolutions, invalidate_chain_cache};
pub use credential::{Credential, CredentialError, CredentialProvider};
pub use providers::*;

/// Forget every cached credential **failure**, process-wide, so the next
/// resolve actually runs its providers again. Cached successes are kept.
///
/// Two independent caches have to be cleared together or neither takes
/// effect: [`CredentialChain`] memoises its own outcome per scope for five
/// minutes, and [`CommandProvider`] separately holds a 30s-to-5min failure
/// backoff per command. The chain consults the command provider from
/// *inside* its own cached region, so clearing only the command cache
/// leaves the chain answering from its copy, and clearing only the chain
/// leaves the command provider refusing to re-run. That asymmetry is why
/// `Shift-R` clearing just the command cache never actually re-resolved a
/// broken token.
///
/// Call this on a **user-initiated** retry — an explicit refresh, or a
/// picker the user just opened — never on a poll tick, whose whole reason
/// for having these caches is to not re-run a chain that keeps failing.
pub fn invalidate_failed_credentials() {
    forget_failed_chain_resolutions();
    forget_failed_command_credentials();
}
