//! Lazy `r`-spawn box lifecycle — lazybox owns the box end to end.
//!
//! The realm `Model` addresses a remote box through an ordinary in-process
//! [`Client`] (from [`lazybox_ipc::channel::pair`]) whose far end is **this worker**,
//! not a live daemon. So a `sandbox:` box being configured is enough to
//! light up the `r <agent>` chords (the pair exists), while a normal launch
//! never touches the provider: the box stays asleep until the first `r`-spawn.
//!
//! When the first command arrives the worker runs the existing sandbox
//! engine — [`connect_box`] does ensure (create if missing) → connect
//! (wake + build the SSH forward) — supervises the forward in-process, dials
//! the box's daemon over the **internally derived** socket, and forwards
//! that command (and every one after) to it. No socket or tunnel is ever
//! configured or shown — the whole transport is derived from the provider
//! configuration and deployment.
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
use lazybox_core::SessionKey;
use lazybox_ipc::{Client, Command, socket};
use lazybox_sandbox::{SandboxProvider, SandboxSpec, connect_box, persist};
use lazybox_store::Store;
use lazybox_tui_core::remote::RemoteBoxNotice;
use tokio::sync::mpsc;

use crate::sandbox::{ResolvedProvider, SHARED_BOX_KEY, resolve_provider, resolve_spec};

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

/// How long to wait for the forward to bind its local socket before giving
/// up on this bring-up. Covers SSH auth + IAP handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Bound on the worker→UI notice channel. Notices are rare (bring-up
/// progress, dropped commands); overflow drops the notice, never blocks
/// the worker.
const NOTICE_CAPACITY: usize = 32;

/// A brought-up remote box the realm `Model` can address: the display name
/// (the deployment name → the sidebar's `⇅ <name>` glyph), the client end
/// of the pair to the worker, and the worker's notice stream (bring-up
/// progress + dropped-command rollbacks) the run loop drains into footer
/// flashes.
pub struct RemoteBox {
    pub name: String,
    pub client: Client,
    pub notices: mpsc::Receiver<RemoteBoxNotice>,
}

/// The per-box local Unix socket the forward binds — the endpoint the
/// worker's box client dials. Dedicated (never the `--connect` socket) so
/// the two never collide, and **per-process** (pid-suffixed): two
/// concurrent lazybox instances would otherwise fight over one path —
/// the second forward can't bind, and the first instance quitting
/// unlinks the socket out from under the second's live connection. Each
/// worker unlinks its own socket on exit; a crashed run's leftover file
/// is inert (nothing dials it) and reclaimed on pid reuse by the
/// supervisor's stale-socket clearing.
fn local_socket() -> PathBuf {
    lazybox_core::paths::state_root()
        .join("remotes")
        .join(format!("sandbox-{}.sock", std::process::id()))
}

/// Build the configured provider for the shared box, deriving the transport
/// internally: a dedicated local socket, and the conventional box daemon
/// socket when `sandbox.remote_socket` is unset (the product path leaves it
/// unset).
fn build_provider(sandbox: &SandboxConfig) -> anyhow::Result<ResolvedProvider> {
    // `resolve_provider` already defaults `remote_socket` to the
    // conventional home-relative box daemon socket; only the local socket
    // is overridden to the dedicated per-box path.
    let mut provider = resolve_provider(sandbox, &mut Vec::new(), SHARED_BOX_KEY)?;
    provider.set_local_socket(local_socket());
    Ok(provider)
}

fn build_spec(sandbox: &SandboxConfig) -> anyhow::Result<SandboxSpec> {
    let provider = resolve_provider(sandbox, &mut Vec::new(), SHARED_BOX_KEY)?;
    resolve_spec(sandbox, &mut Vec::new(), SHARED_BOX_KEY, &provider)
}

/// Wire the lazy worker when a `sandbox:` box is configured, returning the
/// `Model`-facing client + glyph name. `None` — the `r` chords stay hidden
/// and no worker spawns — when the box can't be resolved (no `sandbox:`
/// block, or a GCP block missing a project). Cheap: it resolves config and
/// spawns a task, but never touches the provider (that waits for the first
/// `r`-spawn).
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
    let ports = forward_ports(sandbox, &spec);
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(CHANNEL_CAPACITY);
    let (_box_events_tx, evt_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (notice_tx, notices) = mpsc::channel(NOTICE_CAPACITY);
    let client = Client::from_bounded_channels(cmd_tx, evt_rx);
    tokio::spawn(run(provider, spec, store, ports, cmd_rx, notice_tx));
    Some(RemoteBox {
        name,
        client,
        notices,
    })
}

/// Whether a bring-up failure's root cause is the wire-fingerprint
/// handshake rejecting a daemon built from a different commit (#977). The
/// initial dial (`socket::connect_reconnecting`) surfaces this as an
/// `io::Error` carrying [`lazybox_ipc::socket::HandshakeError::FingerprintMismatch`]'s message
/// (string-wrapped, so it can't be downcast) — matched on the stable
/// "fingerprint mismatch" phrase so the drop becomes an actionable
/// "run `lazybox sandbox rebuild`" notice instead of a generic failure.
fn is_fingerprint_mismatch(err: &anyhow::Error) -> bool {
    format!("{err:#}")
        .to_ascii_lowercase()
        .contains("fingerprint mismatch")
}

/// The workspace a dropped command was going to spawn into, when it was a
/// spawn — the UI rolls that row's optimistic `⇅` tag back.
fn spawn_session_key(cmd: &Command) -> Option<SessionKey> {
    match cmd {
        Command::Spawn { session_key, .. } => Some(session_key.clone()),
        _ => None,
    }
}

/// Fire-and-forget a worker→UI notice. A full or closed channel drops the
/// notice — the UI being gone (or flooded) must never wedge the worker.
fn notify(tx: &mpsc::Sender<RemoteBoxNotice>, notice: RemoteBoxNotice) {
    let _ = tx.try_send(notice);
}

/// Bring the box up on demand: reuse a stamped handle (skip the Terraform
/// apply) or ensure it, connect (wakes a stopped box), supervise the
/// forward, wait for the socket, and dial the box daemon. Returns the box
/// client plus the forward supervisor to hold for the session.
async fn bring_up(
    provider: &ResolvedProvider,
    spec: &SandboxSpec,
    store: &dyn Store,
    ports: &[u16],
) -> anyhow::Result<(Client, tokio::task::JoinHandle<()>)> {
    // Preflight the provider's own credentials so a missing/expired one drops
    // as an actionable UI notice, not a raw terraform/gcloud failure (#1047).
    provider.check_auth().await?;
    if let Some(parent) = provider.state_file().and_then(std::path::Path::parent) {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = provider.local_socket().parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = persist::load_handle_for_provider(store, SHARED_BOX_KEY, provider.id())?;
    if existing.is_none() {
        tracing::info!("provisioning r-spawn box (terraform apply)…");
    }
    let (mut handle, tunnel) = connect_box(provider, spec, existing, ports).await?;
    handle.observe(lazybox_sandbox::PowerState::Running, chrono::Utc::now());
    persist::save_handle(store, SHARED_BOX_KEY, &handle)?;

    let local_socket = tunnel.local_socket.clone();
    let supervisor = tokio::spawn(crate::tunnel::supervise_argv(
        tunnel.program,
        tunnel.args,
        tunnel.env,
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

/// Workload ports the box forwards alongside the daemon socket: the union
/// of `sandbox.ports` (what the CLI's `connect` forwards) and the
/// deployment's declared `workload_ports` — so the `r`-spawn path never
/// silently ignores a configured port the CLI would honor.
fn forward_ports(sandbox: &SandboxConfig, spec: &SandboxSpec) -> Vec<u16> {
    crate::sandbox::union_ports(
        sandbox.ports.clone(),
        &spec.deployment.config.workload_ports,
    )
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
    provider: ResolvedProvider,
    spec: SandboxSpec,
    store: Arc<dyn Store>,
    ports: Vec<u16>,
    mut cmd_rx: mpsc::Receiver<Command>,
    notice_tx: mpsc::Sender<RemoteBoxNotice>,
) {
    let name = spec.deployment.config.name.clone();
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
                        // `TrySendError` hands the command back — name the
                        // workspace whose spawn just died so the UI can
                        // roll its `⇅` tag back.
                        let (mpsc::error::TrySendError::Full(cmd)
                        | mpsc::error::TrySendError::Closed(cmd)) = e;
                        notify(
                            &notice_tx,
                            RemoteBoxNotice::Dropped {
                                session_keys: spawn_session_key(&cmd).into_iter().collect(),
                                error: format!(
                                    "⇅ {name}: command to box dropped (link busy/closed)"
                                ),
                            },
                        );
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
            let stamped = persist::load_handle(store.as_ref(), SHARED_BOX_KEY)
                .ok()
                .flatten()
                .is_some();
            if !stamped {
                // About to provision. If a pre-#965 build stamped a box
                // under a per-worktree key, that instance still exists
                // (and bills) — say so instead of silently paying for two.
                let legacy: Vec<String> = persist::list_handle_keys(store.as_ref())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|k| k != SHARED_BOX_KEY)
                    .collect();
                if !legacy.is_empty() {
                    notify(
                        &notice_tx,
                        RemoteBoxNotice::Info(format!(
                            "⇅ {name}: found older box handle(s) under {} — provisioning a \
                             new shared box; manage the old one with `lazybox sandbox \
                             --worktree <key>`",
                            legacy.join(", ")
                        )),
                    );
                }
            }
            notify(
                &notice_tx,
                RemoteBoxNotice::Info(if stamped {
                    format!("⇅ {name}: waking box…")
                } else {
                    format!(
                        "⇅ {name}: provisioning {} box (first use can take minutes)…",
                        provider.id()
                    )
                }),
            );
            match with_retries(MAX_BRINGUP_ATTEMPTS, BRINGUP_RETRY_BACKOFF, || {
                bring_up(&provider, &spec, store.as_ref(), &ports)
            })
            .await
            {
                Ok((client, supervisor)) => {
                    notify(
                        &notice_tx,
                        RemoteBoxNotice::Info(format!("⇅ {name}: box connected")),
                    );
                    if let Err(e) = client.send(cmd) {
                        tracing::warn!("r-spawn: send to box failed: {e}");
                        let (mpsc::error::TrySendError::Full(cmd)
                        | mpsc::error::TrySendError::Closed(cmd)) = e;
                        notify(
                            &notice_tx,
                            RemoteBoxNotice::Dropped {
                                session_keys: spawn_session_key(&cmd).into_iter().collect(),
                                error: format!(
                                    "⇅ {name}: command to box dropped (link busy/closed)"
                                ),
                            },
                        );
                    }
                    connected = Some((client, supervisor));
                }
                Err(e) => {
                    // The box just proved unreachable after a full retry
                    // cycle. Everything queued behind the triggering
                    // command was riding this same bring-up — running
                    // another multi-attempt cycle per queued command would
                    // burn minutes against a dead box (a 5-row bulk
                    // fan-out ≈ 5 × ~66s of churn). Drain and drop the
                    // whole queue now, with ONE aggregate notice that
                    // rolls back every affected `⇅` tag. A command that
                    // arrives after this drain is a fresh user action and
                    // earns a fresh bring-up attempt.
                    let mut dropped = vec![cmd];
                    while let Ok(queued) = cmd_rx.try_recv() {
                        dropped.push(queued);
                    }
                    let session_keys: Vec<SessionKey> =
                        dropped.iter().filter_map(spawn_session_key).collect();
                    // A fingerprint mismatch is not a transient failure — the
                    // box daemon is a different build. Say so and name the fix
                    // (#977), instead of the generic "bring-up failed" that
                    // reads as flaky infra and buries the actual remedy.
                    let error = if is_fingerprint_mismatch(&e) {
                        tracing::warn!(
                            "r-spawn box daemon fingerprint mismatch; dropping {} command(s): {e:#}",
                            dropped.len()
                        );
                        format!(
                            "⇅ {name}: box daemon is built from a different commit — run \
                             `lazybox sandbox rebuild`, then retry ({} command(s) dropped)",
                            dropped.len()
                        )
                    } else {
                        tracing::warn!(
                            "r-spawn box bring-up failed after retries; dropping {} queued command(s): {e:#}",
                            dropped.len()
                        );
                        format!(
                            "⇅ {name}: box bring-up failed after {MAX_BRINGUP_ATTEMPTS} attempts — {} command(s) dropped ({e:#})",
                            dropped.len()
                        )
                    };
                    notify(
                        &notice_tx,
                        RemoteBoxNotice::Dropped {
                            session_keys,
                            error,
                        },
                    );
                }
            }
        }
    }
    // Worker exit (the Model dropped its client): remove this process's
    // socket file so the per-pid directory doesn't accumulate one dead
    // path per run. Best-effort — a crashed run skips this and leaves an
    // inert file.
    let _ = std::fs::remove_file(provider.local_socket());
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
        assert_eq!(p.local_socket(), local_socket());
        assert!(
            p.local_socket()
                .starts_with(lazybox_core::paths::state_root())
        );
        // Per-process: two concurrent lazybox instances must not fight
        // over one socket path (the second can't bind; the first's exit
        // unlinks it under the second).
        assert!(
            p.local_socket()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&std::process::id().to_string()),
            "{:?} must be pid-scoped",
            p.local_socket()
        );
        assert_eq!(p.remote_socket(), crate::sandbox::BOX_DAEMON_SOCKET);
        // Isolated state, keyed by the shared-box key, under the state root.
        assert!(
            p.state_file()
                .unwrap()
                .starts_with(lazybox_core::paths::state_root())
        );
        assert!(p.state_file().unwrap().ends_with("terraform.tfstate"));
    }

    #[test]
    fn an_explicit_remote_socket_is_kept() {
        let sc = SandboxConfig {
            remote_socket: Some("/custom/daemon.sock".into()),
            ..SandboxConfig::default()
        };
        assert_eq!(
            build_provider(&sc).unwrap().remote_socket(),
            "/custom/daemon.sock"
        );
    }

    #[test]
    fn forward_ports_unions_config_ports_with_the_deployment() {
        // The CLI's `connect` honors `sandbox.ports`; the r-spawn path
        // must not silently ignore them (audit) — it forwards the union,
        // deduplicated.
        let sc = SandboxConfig {
            project: Some("p".into()),
            ports: vec![8082, 3000],
            ..SandboxConfig::default()
        };
        let spec = build_spec(&sc).unwrap();
        let ports = forward_ports(&sc, &spec);
        assert!(ports.contains(&8082), "config-only port kept: {ports:?}");
        for p in &spec.deployment.config.workload_ports {
            assert!(ports.contains(p), "deployment port {p} kept: {ports:?}");
        }
        let mut deduped = ports.clone();
        deduped.dedup();
        assert_eq!(ports, deduped, "no duplicate forwards");
    }

    #[test]
    fn is_fingerprint_mismatch_matches_the_real_handshake_error() {
        // Couple the detector to the ACTUAL ipc error text, wrapped exactly
        // as the dial surfaces it: the handshake `Display` string wrapped in
        // an `io::Error`, then in anyhow — so a reworded ipc message breaks
        // this test rather than silently degrading the notice to a generic
        // "bring-up failed".
        let handshake = lazybox_ipc::socket::HandshakeError::FingerprintMismatch {
            peer: 0x1111_1111,
            ours: 0x2222_2222,
        };
        let io = std::io::Error::other(handshake.to_string());
        let err = anyhow::Error::new(io).context("forward did not dial the box daemon");
        assert!(is_fingerprint_mismatch(&err), "{err:#}");

        // A plain transport failure must NOT be classified as a mismatch.
        let other = anyhow::anyhow!("forward did not bind /tmp/x.sock within 20s");
        assert!(!is_fingerprint_mismatch(&other));
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
    async fn setup_accepts_e2b_without_a_gcp_project() {
        let sc = SandboxConfig {
            provider: Some("e2b".into()),
            ..SandboxConfig::default()
        };
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let rb = setup(&sc, store).expect("an E2B provider does not require a GCP project");
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
