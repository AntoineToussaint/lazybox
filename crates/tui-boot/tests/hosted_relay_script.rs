#![cfg(unix)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lazybox_entitlement::{AccountId, Entitlement, EntitlementError, EntitlementGate};
use lazybox_identity::BoxIdentity;
use lazybox_relay::Relay;
use tokio::net::TcpListener;

struct EnrolledBox(String);

impl EntitlementGate for EnrolledBox {
    fn check<'a>(
        &'a self,
        account: &'a AccountId,
    ) -> Pin<Box<dyn Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>> {
        Box::pin(async move {
            if account.as_str() == self.0 {
                Ok(Entitlement::Active)
            } else {
                Ok(Entitlement::Inactive {
                    reason: "box key is not enrolled".into(),
                })
            }
        })
    }
}

#[tokio::test]
async fn smoke_script_reuses_a_pre_enrolled_box_identity() {
    let box_home = tempfile::tempdir().unwrap();
    let identity_dir = box_home.path().join("v2/identity");
    let identity = BoxIdentity::load_or_generate(&identity_dir).unwrap();
    let expected_key = identity.public_key_base64();

    let key_output = tokio::process::Command::new(env!("CARGO_BIN_EXE_lazybox"))
        .args(["device", "box", "--format", "base64"])
        .env("LAZYBOX_HOME", box_home.path())
        .output()
        .await
        .unwrap();
    assert!(key_output.status.success());
    assert_eq!(
        String::from_utf8(key_output.stdout).unwrap().trim(),
        expected_key
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::with_gate(Box::new(EnrolledBox(expected_key))));
    let relay_task = tokio::spawn(relay.serve(listener));

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/smoke-hosted-relay.sh");
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("bash")
            .arg(script)
            .arg(relay_addr)
            .env("LAZYBOX_BIN", env!("CARGO_BIN_EXE_lazybox"))
            .env("LAZYBOX_SMOKE_BOX_HOME", box_home.path())
            .env_remove("LAZYBOX_HOME")
            .output(),
    )
    .await
    .expect("hosted relay smoke script timed out")
    .unwrap();
    relay_task.abort();

    assert!(
        output.status.success(),
        "smoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("relay smoke passed: encrypted daemon round trip completed")
    );
}
