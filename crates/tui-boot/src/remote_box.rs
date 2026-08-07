//! Lazy `r`-spawn box lifecycle — lazybox owns the box end to end.
//!
//! The realm `Model` addresses a remote box through an ordinary in-process
//! [`Client`] (from [`channel::pair`]) whose far end is **this worker**,
//! not a live daemon. So a `sandbox:` box being configured is enough to
//! light up the `r <agent>` chords (the pair exists), while a normal launch
//! never touches GCP: the box stays asleep until the first `r`-spawn.
//!
//! When the first command arrives the worker runs the existing sandbox
//! engine — [`connect_box`] does ensure (create if missing) → connect
//! (wake + build the IAP forward) — supervises the forward in-process, dials
//! the box's daemon over the **internally derived** socket, and forwards
//! that command (and every one after) to it; the box's events flow back
//! through the pair to the `Model`. No socket or tunnel is ever configured
//! or shown — the whole transport is derived from `{project, deployment}`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lazybox_config::SandboxConfig;
use lazybox_ipc::{Client, Command, Event, socket};
use lazybox_sandbox::gcp::GcpProvider;
use lazybox_sandbox::{SandboxSpec, connect_box, persist};
use lazybox_store::Store;
use tokio::sync::mpsc;

use crate::sandbox::{resolve_provider, resolve_spec};

/// Bound on the in-process command/event channels bridging the `Model` to
/// this worker — the same order as the IPC transport's own channels.
const CHANNEL_CAPACITY: usize = 256;

/// The single shared `r`-spawn box's stable identity — a fixed key, not a
/// worktree, so the instance name and persisted handle are the same across
/// every launch and every workspace that spawns onto it.
const SHARED_BOX_KEY: &str = "sandbox";

/// The box's daemon socket, **relative** to the SSH login home: `ssh -L`
/// resolves a relative remote socket against the box user's `$HOME`, so
/// lazybox forwards to it without knowing that path. Matches
/// `contrib/box-lifecycle/connect.sh` (`LAZYBOX_BOX_SOCK` default). Used
/// only when `sandbox.remote_socket` is unset — the product path sets
/// nothing.
const BOX_DAEMON_SOCKET: &str = ".lazybox/run/daemon.sock";

/// How long to wait for the forward to bind its local socket before giving
/// up on this bring-up. Covers SSH auth + IAP handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// A brought-up remote box the realm `Model` can address: the display name
/// (the deployment name → the sidebar's `⇅ <name>` glyph) and the client
/// end of the pair to the worker.
pub struct RemoteBox {
    pub name: String,
    pub client: Client,
}

/// The per-box local Unix socket the forward binds — the endpoint the
/// worker's box client dials. Dedicated (never the `--connect` socket) so
/// the two never collide.
fn local_socket() -> PathBuf {
    lazybox_core::paths::state_root()
        .join("remotes")
        .join("sandbox.sock")
}

/// Build the GCP provider for the shared box, deriving the transport
/// internally: a dedicated local socket, and the conventional box daemon
/// socket when `sandbox.remote_socket` is unset (the product path leaves it
/// unset).
fn build_provider(sandbox: &SandboxConfig) -> anyhow::Result<GcpProvider> {
    let mut provider = resolve_provider(sandbox, &mut Vec::new(), SHARED_BOX_KEY)?;
    provider.local_socket = local_socket();
    if provider.remote_socket.is_empty() {
        provider.remote_socket = BOX_DAEMON_SOCKET.to_string();
    }
    Ok(provider)
}

fn build_spec(sandbox: &SandboxConfig) -> anyhow::Result<SandboxSpec> {
    resolve_spec(sandbox, &mut Vec::new(), SHARED_BOX_KEY)
}

/// Wire the lazy worker when a `sandbox:` box is configured, returning the
/// `Model`-facing client + glyph name. `None` — the `r` chords stay hidden
/// and no worker spawns — when the box can't be resolved (no `sandbox:`
/// block, or one missing a project). Cheap: it resolves config and spawns a
/// task, but never touches GCP (that waits for the first `r`-spawn).
pub fn setup(sandbox: &SandboxConfig, store: Arc<dyn Store>) -> Option<RemoteBox> {
    let provider = build_provider(sandbox)
        .map_err(|e| tracing::info!("no r-spawn box: {e:#}"))
        .ok()?;
    let spec = build_spec(sandbox)
        .map_err(|e| tracing::info!("no r-spawn box: {e:#}"))
        .ok()?;
    let name = spec.deployment.config.name.clone();
    // A plain channel pair, not `channel::pair()`: that one needs a
    // server-driven event forwarder. The worker owns the far end and
    // relays to/from the box directly.
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(CHANNEL_CAPACITY);
    let (evt_tx, evt_rx) = mpsc::channel::<Event>(CHANNEL_CAPACITY);
    let client = Client::from_bounded_channels(cmd_tx, evt_rx);
    tokio::spawn(run(provider, spec, store, cmd_rx, evt_tx));
    Some(RemoteBox { name, client })
}

/// Bring the box up on demand: reuse a stamped handle (skip the Terraform
/// apply) or ensure it, connect (wakes a stopped box), supervise the
/// forward, wait for the socket, and dial the box daemon. Returns the box
/// client plus the forward supervisor to hold for the session.
async fn bring_up(
    provider: &GcpProvider,
    spec: &SandboxSpec,
    store: &dyn Store,
) -> anyhow::Result<(Client, tokio::task::JoinHandle<()>)> {
    if let Some(parent) = provider.state_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = provider.local_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = persist::load_handle(store, SHARED_BOX_KEY)?;
    if existing.is_none() {
        tracing::info!("provisioning r-spawn box (terraform apply)…");
    }
    let (mut handle, tunnel) = connect_box(provider, spec, existing, &spec_ports(spec)).await?;
    handle.observe(lazybox_sandbox::PowerState::Running, chrono::Utc::now());
    persist::save_handle(store, SHARED_BOX_KEY, &handle)?;

    let local_socket = tunnel.local_socket.clone();
    let supervisor = tokio::spawn(crate::tunnel::supervise_argv(
        tunnel.program,
        tunnel.args,
        local_socket.clone(),
    ));
    if !crate::tunnel::wait_for_socket(&local_socket, CONNECT_TIMEOUT).await {
        supervisor.abort();
        anyhow::bail!(
            "forward did not bind {} within {}s",
            local_socket.display(),
            CONNECT_TIMEOUT.as_secs()
        );
    }
    let (client, _peer) = socket::connect_reconnecting(&local_socket).await?;
    Ok((client, supervisor))
}

/// Workload ports the box forwards alongside the daemon socket — the
/// deployment's declared ports (a dev server the user may want to reach).
fn spec_ports(spec: &SandboxSpec) -> Vec<u16> {
    spec.deployment.config.workload_ports.clone()
}

/// The worker loop: hold the box link once up, relaying the `Model`'s
/// commands to it and its events back. Idle (no GCP) until the first
/// command; a failed bring-up drops that command and waits for the next to
/// retry, so a transient failure doesn't permanently disable the box.
/// Draining the box link's events into `evt_tx` also keeps that link from
/// backing up (the `Model`-side render of those events is a follow-up).
async fn run(
    provider: GcpProvider,
    spec: SandboxSpec,
    store: Arc<dyn Store>,
    mut cmd_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<Event>,
) {
    let mut connected: Option<(Client, tokio::task::JoinHandle<()>)> = None;
    loop {
        if let Some((box_client, _guard)) = &mut connected {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    // Model gone → tear down (dropping the guard kills the forward).
                    None => break,
                    Some(cmd) => {
                        if let Err(e) = box_client.send(cmd) {
                            tracing::warn!("r-spawn: send to box failed: {e}");
                        }
                    }
                },
                evt = box_client.rx.recv() => match evt {
                    // Box link closed → drop it; the next command re-brings-up.
                    None => connected = None,
                    Some(evt) => {
                        let _ = evt_tx.try_send(evt);
                    }
                },
            }
        } else {
            let Some(cmd) = cmd_rx.recv().await else {
                break;
            };
            match bring_up(&provider, &spec, store.as_ref()).await {
                Ok((client, guard)) => {
                    if let Err(e) = client.send(cmd) {
                        tracing::warn!("r-spawn: send to box failed: {e}");
                    }
                    connected = Some((client, guard));
                }
                Err(e) => tracing::warn!("r-spawn box bring-up failed: {e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_derives_a_dedicated_local_and_convention_remote_socket() {
        // The product path leaves remote_socket unset → the box-relative
        // convention; the local socket is dedicated under the state root,
        // never the `--connect` socket.
        let sc = SandboxConfig {
            project: Some("p".into()),
            ..SandboxConfig::default()
        };
        let p = build_provider(&sc).unwrap();
        assert_eq!(p.local_socket, local_socket());
        assert!(
            p.local_socket
                .starts_with(lazybox_core::paths::state_root())
        );
        assert_eq!(p.remote_socket, BOX_DAEMON_SOCKET);
        // Isolated state, keyed by the shared-box key, under the state root.
        assert!(p.state_file.starts_with(lazybox_core::paths::state_root()));
        assert!(p.state_file.ends_with("terraform.tfstate"));
    }

    #[test]
    fn an_explicit_remote_socket_is_kept() {
        let sc = SandboxConfig {
            remote_socket: Some("/custom/daemon.sock".into()),
            ..SandboxConfig::default()
        };
        assert_eq!(
            build_provider(&sc).unwrap().remote_socket,
            "/custom/daemon.sock"
        );
    }

    #[tokio::test]
    async fn setup_is_none_without_a_configured_box() {
        // No project → no spec → no box, so the `r` chords stay hidden and
        // no worker spawns. The default `sandbox:` block has no project.
        let store = Arc::new(lazybox_store::MemoryStore::new());
        assert!(setup(&SandboxConfig::default(), store).is_none());
    }

    #[tokio::test]
    async fn setup_yields_a_box_named_after_the_deployment() {
        // A minimal but valid `sandbox:` block → a box whose glyph name is
        // the (default) deployment name, and a live client end.
        let sc = SandboxConfig {
            project: Some("proj".into()),
            ..SandboxConfig::default()
        };
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let rb = setup(&sc, store).expect("a configured box yields a RemoteBox");
        assert_eq!(rb.name, "default");
    }
}
