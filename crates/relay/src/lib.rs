//! Rendezvous relay for lazybox's BYOR remote access.
//!
//! A box dials **out** to a codefly-hosted relay and holds the
//! connection open — behind NAT, with no inbound ports, DNS, or certs.
//! The relay brokers a client to that box by box-id and forwards
//! **ciphertext only**; it executes nothing and, once the E2E channel
//! (#891) wraps the stream, never sees plaintext.
//!
//! - [`server::Relay`] — the codefly-hosted broker (the `lazybox-relay`
//!   binary).
//! - [`client::serve_box`] — the box side (`lazybox serve` dials out with
//!   this).
//! - [`client::connect_through_relay`] — the client side.

pub mod client;
pub mod protocol;
pub mod server;

pub use client::{
    OnClient, RelayClientError, SUBSCRIPTION_REQUIRED_MESSAGE, connect_through_relay, serve_box,
};
pub use protocol::{Ack, Hello, RegistrationChallenge, RegistrationProof, ToBox};
pub use server::Relay;

// Test-only config sandbox for this unit-test binary (#1539, #1751). The
// same body lives in the crate's `tests/common/mod.rs` and in every other
// test binary that can reach `lazybox-config`; `crates/core/tests/
// test_isolation.rs` requires one per binary, and a shared helper would
// put an env-mutating function on a production API surface. Keep the
// copies identical.
#[cfg(test)]
mod config_sandbox {
    /// Point `LAZYBOX_HOME` at a throwaway dir so every `Config::load()` in
    /// this test binary resolves to defaults instead of the developer's
    /// real config. Installed from a before-main `#[ctor]`, the one hook
    /// that beats the test harness to every test. Unique per process run
    /// (pid + start nanos) so a recycled pid can never make a later run
    /// read a stale sandbox.
    fn install() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "lazybox-relay-config-sandbox-{}-{}",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: a `#[ctor]` runs before `main`, while the process is
        // still single-threaded and no other initializer in this binary
        // spawns a thread — so nothing can race this env write.
        unsafe { std::env::set_var("LAZYBOX_HOME", &dir) };
        lazybox_config::Config::invalidate_cache();
    }

    #[ctor::ctor]
    unsafe fn redirect_config_home() {
        install();
    }

    /// The redirect must actually be in force in this binary. Hermetic —
    /// the real file is never read, and the check holds under a sibling
    /// test's own pinned home too, since that is not the real profile
    /// either.
    #[test]
    fn config_path_resolves_to_a_sandbox_not_the_real_home() {
        let real = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"))
            .join(".lazybox")
            .join("config.yaml");
        assert_ne!(
            lazybox_config::Config::default_path(),
            real,
            "LAZYBOX_HOME redirect is not active — this binary can reach the real config"
        );
    }
}
