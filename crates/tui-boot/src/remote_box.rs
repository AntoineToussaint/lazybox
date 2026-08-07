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
//! that command (and every one after) to it. No socket or tunnel is ever
//! configured or shown — the whole transport is derived from
//! `{project, deployment}`.
//!
//! This is the **command path**: an `r`-spawn reaches the box and the box
//! runs the session. Rendering the box's *own* terminal/session state back
//! into this `Model` is deliberately **not** wired — a second daemon's
//! `Event::Snapshot` is authoritative in the shared event handler and would
//! prune the local inbox (`events.rs`), so merging two daemons' state is a
//! follow-up, not a drop-in drain. The worker therefore drains the box
//! link's events only to keep it healthy, and discards them.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lazybox_config::SandboxConfig;
use lazybox_ipc::{Client, Command, socket};
use lazybox_sandbox::gcp::GcpProvider;
use lazybox_sandbox::{SandboxSpec, connect_box, persist};
use lazybox_store::Store;
use tokio::sync::mpsc;

use crate::sandbox::{resolve_provider, resolve_spec};

/// Bound on the in-process command channel bridging the `Model` to this
/// worker — the same order as the IPC transport's own channels.
const CHANNEL_CAPACITY: usize = 256;

/// How many times to retry a box bring-up for the command that triggered
/// it before giving up. A transient IAP/ssh/handshake failure on the first
/// `r`-spawn must not silently drop that spawn.
const MAX_BRINGUP_ATTEMPTS: usize = 3;

/// Delay between bring-up attempts. Short — the slow part (Terraform, wake)
/// is inside `bring_up`; this only spaces out a flaky connect/handshake.
const BRINGUP_RETRY_BACKOFF: Duration = Duration::from_secs(2);

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
    // A plain channel, not `channel::pair()`: that one needs a
    // server-driven event forwarder. The worker owns the far end and
    // relays commands to the box directly. The event half is left
    // unwired — the `Model` never drains a remote client's events (and
    // draining the box's would clobber the local inbox, see the module
    // doc), so the box→`Model` render is a follow-up. `evt_rx` exists
    // only because the `Client` constructor needs one; dropping the
    // sender leaves it permanently empty, which is exactly right.
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(CHANNEL_CAPACITY);
    let (_box_events_tx, evt_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let client = Client::from_bounded_channels(cmd_tx, evt_rx);
    tokio::spawn(run(provider, spec, store, cmd_rx));
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
    // Abort the forward on a failed dial too, or the supervisor task
    // outlives this failed attempt and races the next one over the socket.
    match socket::connect_reconnecting(&local_socket).await {
        Ok((client, _peer)) => Ok((client, supervisor)),
        Err(e) => {
            supervisor.abort();
            Err(e.into())
        }
    }
}

/// Retry `op` up to `attempts` times, spacing failures by `backoff`.
/// Returns the first success or the last error. Used to keep a transient
/// bring-up failure (flaky connect/handshake) from silently dropping the
/// `r`-spawn that triggered it.
async fn with_retries<T, F, Fut>(attempts: usize, backoff: Duration, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut last_err = None;
    for attempt in 1..=attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                tracing::warn!("r-spawn box bring-up attempt {attempt}/{attempts} failed: {e:#}");
                last_err = Some(e);
                if attempt < attempts {
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
    Err(last_err.expect("attempts >= 1 always records an error on the failure path"))
}

/// Workload ports the box forwards alongside the daemon socket — the
/// deployment's declared ports (a dev server the user may want to reach).
fn spec_ports(spec: &SandboxSpec) -> Vec<u16> {
    spec.deployment.config.workload_ports.clone()
}

/// Abort a forward supervisor. Dropping the `JoinHandle` only **detaches**
/// the task — the `gcloud`/`ssh` forward would keep running and the next
/// bring-up would spawn a second one racing it over the same local socket.
/// Aborting stops it (and, via `kill_on_drop`, its child).
fn stop_forward(supervisor: tokio::task::JoinHandle<()>) {
    supervisor.abort();
}

/// What the worker's select resolved to when a box link is live.
enum Step {
    /// A `Model` command to forward to the box.
    Forward(Command),
    /// The `Model` dropped its client — the worker should exit.
    ModelGone,
    /// The box link closed terminally — tear it down and re-bring-up next.
    BoxClosed,
    /// A box event was drained (and discarded — see the module doc).
    Drained,
}

/// The worker loop: hold the box link once up, relaying the `Model`'s
/// commands to it. Idle (no GCP) until the first command; a failed bring-up
/// (after bounded retries) drops that command and waits for the next to
/// retry, so a transient failure doesn't permanently disable the box. The
/// box link's events are drained to keep it from backing up, then discarded
/// (rendering them is a follow-up — see the module doc).
async fn run(
    provider: GcpProvider,
    spec: SandboxSpec,
    store: Arc<dyn Store>,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    let mut connected: Option<(Client, tokio::task::JoinHandle<()>)> = None;
    loop {
        if let Some((mut box_client, supervisor)) = connected.take() {
            let step = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None => Step::ModelGone,
                    Some(cmd) => Step::Forward(cmd),
                },
                evt = box_client.rx.recv() => match evt {
                    None => Step::BoxClosed,
                    Some(_evt) => Step::Drained,
                },
            };
            match step {
                Step::Forward(cmd) => {
                    if let Err(e) = box_client.send(cmd) {
                        tracing::warn!("r-spawn: send to box failed: {e}");
                    }
                    connected = Some((box_client, supervisor));
                }
                Step::Drained => connected = Some((box_client, supervisor)),
                // Terminal link close → abort the forward before re-bringing
                // up, or two supervisors race the socket.
                Step::BoxClosed => stop_forward(supervisor),
                Step::ModelGone => {
                    stop_forward(supervisor);
                    break;
                }
            }
        } else {
            let Some(cmd) = cmd_rx.recv().await else {
                break;
            };
            match with_retries(MAX_BRINGUP_ATTEMPTS, BRINGUP_RETRY_BACKOFF, || {
                bring_up(&provider, &spec, store.as_ref())
            })
            .await
            {
                Ok((client, supervisor)) => {
                    if let Err(e) = client.send(cmd) {
                        tracing::warn!("r-spawn: send to box failed: {e}");
                    }
                    connected = Some((client, supervisor));
                }
                Err(e) => tracing::warn!("r-spawn box bring-up failed after retries: {e:#}"),
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

    #[tokio::test]
    async fn stop_forward_aborts_the_supervisor_not_just_detaches_it() {
        // The leak this guards: a mere `drop` of the JoinHandle detaches the
        // task, leaving the `gcloud`/`ssh` forward running to race the next
        // bring-up. `stop_forward` must abort it.
        let supervisor = tokio::spawn(async {
            // Only an abort ends this; a detach would leave it running.
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        let probe = supervisor.abort_handle();
        assert!(!probe.is_finished(), "task is live before teardown");
        stop_forward(supervisor);
        for _ in 0..100 {
            if probe.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            probe.is_finished(),
            "stop_forward must abort the supervisor, not detach it"
        );
    }

    #[tokio::test]
    async fn with_retries_recovers_from_a_transient_failure() {
        // A bring-up that fails twice then succeeds must not drop the
        // triggering command — with_retries keeps trying to the cap.
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let out = with_retries(3, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 3 {
                    anyhow::bail!("transient")
                } else {
                    Ok(n)
                }
            }
        })
        .await
        .expect("succeeds on the third attempt");
        assert_eq!(out, 3);
        assert_eq!(calls.get(), 3, "retried until success");
    }

    #[tokio::test]
    async fn with_retries_gives_up_at_the_attempt_cap() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let result: anyhow::Result<()> = with_retries(3, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            async { anyhow::bail!("always fails") }
        })
        .await;
        assert!(
            result.is_err(),
            "a permanent failure surfaces the last error"
        );
        assert_eq!(calls.get(), 3, "bounded — stops at the cap, not forever");
    }
}
