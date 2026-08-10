//! The reusable box bring-up: `ensure` (create if missing) → `connect`
//! (which wakes a stopped box). Split out so the TUI's `r`-spawn and the
//! `lazybox sandbox connect` CLI drive the **same** sequence against the
//! existing [`SandboxProvider`] engine rather than each re-implementing it.

use crate::{BoxHandle, SandboxError, SandboxProvider, SandboxSpec, Tunnel};

/// Bring a box up and return its live handle plus the forward to
/// supervise.
///
/// `existing` is the persisted [`BoxHandle`] when the box was already
/// stamped — pass it to skip `ensure`'s Terraform apply; `None` provisions
/// the box first. Either way [`connect`](SandboxProvider::connect) wakes a
/// stopped box before handing back the forward, so the result is always a
/// running box and a ready-to-spawn [`Tunnel`].
pub async fn connect_box<P: SandboxProvider>(
    provider: &P,
    spec: &SandboxSpec,
    existing: Option<BoxHandle>,
    ports: &[u16],
) -> Result<(BoxHandle, Tunnel), SandboxError> {
    let handle = match existing {
        Some(handle) => handle,
        None => provider.ensure(spec).await?,
    };
    let tunnel = provider.connect(&handle, ports).await?;
    Ok((handle, tunnel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoxStatus, Deployment, PowerState};
    use std::cell::Cell;
    use std::path::PathBuf;

    /// A provider that records whether `ensure`/`connect` ran, so the
    /// bring-up sequencing is tested without a real GCP project.
    #[derive(Default)]
    struct FakeProvider {
        ensured: Cell<bool>,
        connected: Cell<bool>,
    }

    fn handle(id: &str) -> BoxHandle {
        BoxHandle {
            provider: "fake".into(),
            id: id.into(),
            region: "r".into(),
            zone: "z".into(),
            project: "p".into(),
            power_state: PowerState::Stopped,
            last_active: None,
        }
    }

    impl SandboxProvider for FakeProvider {
        fn id(&self) -> &str {
            "fake"
        }
        async fn ensure(&self, _spec: &SandboxSpec) -> Result<BoxHandle, SandboxError> {
            self.ensured.set(true);
            Ok(handle("ensured"))
        }
        async fn start(&self, _h: &BoxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn stop(&self, _h: &BoxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn status(&self, _h: &BoxHandle) -> Result<BoxStatus, SandboxError> {
            Ok(BoxStatus {
                power: PowerState::Running,
                reachable: true,
            })
        }
        async fn connect(&self, h: &BoxHandle, ports: &[u16]) -> Result<Tunnel, SandboxError> {
            self.connected.set(true);
            Ok(Tunnel {
                program: "gcloud".into(),
                args: vec![h.id.clone()],
                local_socket: PathBuf::from("/tmp/box.sock"),
                ports: ports.to_vec(),
            })
        }
        async fn destroy(&self, _h: &BoxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            provider: "fake".into(),
            name: "b".into(),
            project: "p".into(),
            region: "r".into(),
            zone: "z".into(),
            deployment: Deployment::default_recipe().unwrap(),
        }
    }

    #[tokio::test]
    async fn provisions_when_no_handle_then_connects() {
        let p = FakeProvider::default();
        let (handle, tunnel) = connect_box(&p, &spec(), None, &[3000]).await.unwrap();
        assert!(p.ensured.get(), "no persisted handle → ensure runs");
        assert!(p.connected.get(), "connect always runs");
        assert_eq!(handle.id, "ensured");
        assert_eq!(tunnel.ports, vec![3000]);
    }

    #[tokio::test]
    async fn reuses_a_stamped_handle_and_skips_ensure() {
        let p = FakeProvider::default();
        let (handle, _tunnel) = connect_box(&p, &spec(), Some(handle("stamped")), &[])
            .await
            .unwrap();
        assert!(
            !p.ensured.get(),
            "a persisted handle skips the Terraform apply"
        );
        assert!(p.connected.get(), "connect still wakes + forwards the box");
        assert_eq!(handle.id, "stamped");
    }
}
