//! Lazy `r`-spawn box lifecycle — lazybox owns the box end to end.
//!
//! The realm `Model` addresses a remote box through an ordinary in-process
//! [`Client`] (from [`lazybox_ipc::channel::pair`]) whose far end is **this worker**,
//! not a live daemon. So a `sandbox:` box being configured is enough to
//! light up the `r <agent>` chords (the pair exists).
//!
//! The box is brought up on demand by any of three triggers: an explicit
//! [`RemoteControl::Connect`] (the `Shift-C` action), the startup
//! auto-connect when opted in with `sandbox.auto_connect: true`, or the first
//! `r`-spawn command. Whichever arrives first, the worker runs the existing
//! sandbox engine — [`connect_box`] does ensure (create if missing) → connect
//! (wake + build the SSH forward) — supervises the forward in-process, dials
//! the box's daemon over the **internally derived** socket, and forwards
//! commands to it. No socket or tunnel is ever configured or shown — the whole
//! transport is derived from the provider configuration and deployment. A
//! bring-up in flight is cancellable by an explicit
//! [`RemoteControl::Disconnect`], so connection is a first-class, responsive
//! action rather than a hidden per-spawn effect (#1066).
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
use lazybox_tui_core::remote::{RemoteBoxNotice, RemoteConnState, RemoteControl};
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
    /// UI→worker control channel: explicit connect/disconnect (and the
    /// startup auto-connect) so connection is a first-class action rather
    /// than a side-effect of the first `r`-spawn (#1066).
    pub control: mpsc::Sender<RemoteControl>,
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
    let (control, control_rx) = mpsc::channel::<RemoteControl>(CHANNEL_CAPACITY);
    let client = Client::from_bounded_channels(cmd_tx, evt_rx);
    tokio::spawn(run(
        provider, spec, store, ports, cmd_rx, control_rx, notice_tx,
    ));
    Some(RemoteBox {
        name,
        client,
        notices,
        control,
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

/// Push a durable connection-state transition to the UI's persistent
/// indicator (#1066).
fn set_state(tx: &mpsc::Sender<RemoteBoxNotice>, state: RemoteConnState) {
    notify(tx, RemoteBoxNotice::State(state));
}

/// A concise, actionable reason for the persistent `error: …` indicator.
/// A fingerprint mismatch names its fix (#977); everything else surfaces
/// the error's top line, kept short since the indicator is width-capped
/// (the full text still rides the transient `Dropped` flash + messages log).
fn conn_error_reason(err: &anyhow::Error) -> String {
    if is_fingerprint_mismatch(err) {
        "daemon build mismatch — run `lazybox sandbox rebuild`".to_string()
    } else {
        err.to_string()
    }
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
enum LiveStep {
    /// A `Model` command to forward to the box.
    Forward(Command),
    /// An explicit disconnect request — drop the link (box keeps running).
    Disconnect,
    /// The `Model` dropped its channels — the worker should exit.
    ModelGone,
    /// The box link closed terminally — tear it down and re-bring-up next.
    BoxClosed,
    /// A box event was drained (and discarded — see the module doc), or a
    /// redundant `Connect` arrived while already connected. Keep the link.
    Keep,
}

/// What the worker's select resolved to while idle (no live link).
enum IdleStep {
    /// A `Model` command — bring the box up, then forward it.
    Command(Command),
    /// An explicit connect (or the startup auto-connect) — bring the box
    /// up with no command to forward.
    Connect,
    /// An explicit disconnect while already down — reassert the state.
    Disconnect,
    /// The `Model` dropped its channels — the worker should exit.
    ModelGone,
}

/// Bring the box up, emitting the durable connection-state transitions the
/// UI's persistent indicator renders (#1066): `creating…` / `waking…`
/// before the (bounded-retry) bring-up, then `connected` on success. On
/// failure it emits `error: …` and returns the error so the caller can also
/// roll back any command that was riding this bring-up.
async fn establish(
    provider: &ResolvedProvider,
    spec: &SandboxSpec,
    store: &dyn Store,
    ports: &[u16],
    notice_tx: &mpsc::Sender<RemoteBoxNotice>,
) -> anyhow::Result<(Client, tokio::task::JoinHandle<()>)> {
    let name = spec.deployment.config.name.clone();
    let stamped = persist::load_handle(store, SHARED_BOX_KEY)
        .ok()
        .flatten()
        .is_some();
    if !stamped {
        // About to provision. If a pre-#965 build stamped a box under a
        // per-worktree key, that instance still exists (and bills) — say
        // so instead of silently paying for two.
        let legacy: Vec<String> = persist::list_handle_keys(store)
            .unwrap_or_default()
            .into_iter()
            .filter(|k| k != SHARED_BOX_KEY)
            .collect();
        if !legacy.is_empty() {
            notify(
                notice_tx,
                RemoteBoxNotice::Info(format!(
                    "⇅ {name}: found older box handle(s) under {} — provisioning a \
                     new shared box; manage the old one with `lazybox sandbox \
                     --worktree <key>`",
                    legacy.join(", ")
                )),
            );
        }
    }
    set_state(
        notice_tx,
        if stamped {
            RemoteConnState::Waking
        } else {
            RemoteConnState::Creating
        },
    );
    match with_retries(MAX_BRINGUP_ATTEMPTS, BRINGUP_RETRY_BACKOFF, || {
        bring_up(provider, spec, store, ports)
    })
    .await
    {
        Ok(up) => {
            set_state(notice_tx, RemoteConnState::Connected { name });
            Ok(up)
        }
        Err(e) => {
            set_state(
                notice_tx,
                RemoteConnState::Error {
                    reason: conn_error_reason(&e),
                },
            );
            Err(e)
        }
    }
}

/// Outcome of racing a bring-up against the control channel.
enum Raced<T> {
    /// The future finished on its own.
    Completed(T),
    /// An explicit `Disconnect` arrived and cancelled the future.
    Cancelled,
    /// The `Model` dropped its control channel — the worker should exit.
    ModelGone,
}

/// Await `fut`, but let an explicit `Disconnect` on `control_rx` cancel it
/// mid-flight (#1066). Without this, `Disconnect` sits queued behind a
/// multi-minute `establish` and the user's cancel appears to do nothing for
/// minutes while GCP keeps provisioning — with the indicator optimistically
/// reading `disconnected` the whole time. A redundant `Connect` is ignored
/// and keeps awaiting the SAME future (dropping it would restart Terraform).
async fn race_control<F, T>(fut: F, control_rx: &mut mpsc::Receiver<RemoteControl>) -> Raced<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Raced::Completed(out),
            ctrl = control_rx.recv() => match ctrl {
                None => return Raced::ModelGone,
                Some(RemoteControl::Disconnect) => return Raced::Cancelled,
                Some(RemoteControl::Connect) => continue,
            },
        }
    }
}

/// Emit the aggregate `Dropped` rollback for a bring-up that failed with
/// commands riding it: one notice rolls back every affected `⇅` tag, and a
/// fingerprint mismatch names its fix (#977) instead of reading as flaky
/// infra.
fn drop_queued_commands(
    name: &str,
    notice_tx: &mpsc::Sender<RemoteBoxNotice>,
    err: &anyhow::Error,
    dropped: Vec<Command>,
) {
    let session_keys: Vec<SessionKey> = dropped.iter().filter_map(spawn_session_key).collect();
    let error = if is_fingerprint_mismatch(err) {
        tracing::warn!(
            "r-spawn box daemon fingerprint mismatch; dropping {} command(s): {err:#}",
            dropped.len()
        );
        format!(
            "⇅ {name}: box daemon is built from a different commit — run \
             `lazybox sandbox rebuild`, then retry ({} command(s) dropped)",
            dropped.len()
        )
    } else {
        tracing::warn!(
            "r-spawn box bring-up failed after retries; dropping {} queued command(s): {err:#}",
            dropped.len()
        );
        format!(
            "⇅ {name}: box bring-up failed after {MAX_BRINGUP_ATTEMPTS} attempts — {} command(s) dropped ({err:#})",
            dropped.len()
        )
    };
    notify(
        notice_tx,
        RemoteBoxNotice::Dropped {
            session_keys,
            error,
        },
    );
}

/// Collect `first` plus every command still queued behind it. A bring-up
/// that ends without a live link (failed OR cancelled) must handle the whole
/// fan-out riding it at once: leaving the queue behind would re-trigger a
/// fresh bring-up per queued command on the next loop turn — burning minutes
/// against a dead box, or silently re-connecting the box the user just
/// cancelled (#1066).
fn drain_pending(first: Command, cmd_rx: &mut mpsc::Receiver<Command>) -> Vec<Command> {
    let mut all = vec![first];
    while let Ok(queued) = cmd_rx.try_recv() {
        all.push(queued);
    }
    all
}

/// Roll back the spawns whose bring-up the user cancelled — the sessions they
/// advertised will never exist, so the `⇅` glyph must not lie (#1066). One
/// aggregate notice covers a whole cancelled fan-out.
fn drop_cancelled_commands(
    name: &str,
    notice_tx: &mpsc::Sender<RemoteBoxNotice>,
    cancelled: &[Command],
) {
    let session_keys: Vec<SessionKey> = cancelled.iter().filter_map(spawn_session_key).collect();
    if session_keys.is_empty() {
        return;
    }
    let error = format!(
        "⇅ {name}: connect cancelled — {} spawn(s) dropped",
        session_keys.len()
    );
    notify(
        notice_tx,
        RemoteBoxNotice::Dropped {
            session_keys,
            error,
        },
    );
}

/// Forward a command to the live box link, rolling back its optimistic `⇅`
/// tag with a `Dropped` notice if the send fails (link busy/closed).
fn forward(name: &str, client: &Client, notice_tx: &mpsc::Sender<RemoteBoxNotice>, cmd: Command) {
    if let Err(e) = client.send(cmd) {
        tracing::warn!("r-spawn: send to box failed: {e}");
        // `TrySendError` hands the command back — name the workspace whose
        // spawn just died so the UI can roll its `⇅` tag back.
        let (mpsc::error::TrySendError::Full(cmd) | mpsc::error::TrySendError::Closed(cmd)) = e;
        notify(
            notice_tx,
            RemoteBoxNotice::Dropped {
                session_keys: spawn_session_key(&cmd).into_iter().collect(),
                error: format!("⇅ {name}: command to box dropped (link busy/closed)"),
            },
        );
    }
}

/// The worker loop: hold the box link once up, relaying the `Model`'s
/// commands to it. Idle (no GCP) until an explicit connect or the first
/// command; a failed bring-up (after bounded retries) drops that command
/// and waits for the next to retry, so a transient failure doesn't
/// permanently disable the box. The box link's events are drained to keep
/// it from backing up, then discarded (rendering them is a follow-up — see
/// the module doc).
async fn run(
    provider: ResolvedProvider,
    spec: SandboxSpec,
    store: Arc<dyn Store>,
    ports: Vec<u16>,
    mut cmd_rx: mpsc::Receiver<Command>,
    mut control_rx: mpsc::Receiver<RemoteControl>,
    notice_tx: mpsc::Sender<RemoteBoxNotice>,
) {
    let name = spec.deployment.config.name.clone();
    // The `Model` paints the initial `disconnected` state itself (it knows a
    // box is configured before the worker emits anything). The worker must
    // NOT re-assert it here: with auto-connect on, a `Connect` is already
    // queued and the optimistic `connecting…` would flicker back to
    // `disconnected` before the bring-up's first state lands (#1066).
    let mut connected: Option<(Client, tokio::task::JoinHandle<()>)> = None;
    loop {
        if let Some((mut box_client, supervisor)) = connected.take() {
            let step = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None => LiveStep::ModelGone,
                    Some(cmd) => LiveStep::Forward(cmd),
                },
                ctrl = control_rx.recv() => match ctrl {
                    None => LiveStep::ModelGone,
                    Some(RemoteControl::Disconnect) => LiveStep::Disconnect,
                    Some(RemoteControl::Connect) => LiveStep::Keep,
                },
                evt = box_client.rx.recv() => match evt {
                    None => LiveStep::BoxClosed,
                    Some(_evt) => LiveStep::Keep,
                },
            };
            match step {
                LiveStep::Forward(cmd) => {
                    forward(&name, &box_client, &notice_tx, cmd);
                    connected = Some((box_client, supervisor));
                }
                LiveStep::Keep => connected = Some((box_client, supervisor)),
                // Explicit disconnect: drop the tunnel; the box keeps
                // running so a later reconnect is cheap.
                LiveStep::Disconnect => {
                    stop_forward(supervisor);
                    set_state(&notice_tx, RemoteConnState::Disconnected);
                }
                // Terminal link close (an unexpected drop — wifi, tunnel
                // reset). Abort the forward before returning to idle, or two
                // supervisors race the socket. Recovery is user-initiated:
                // the indicator shows `disconnected` and the next explicit
                // connect (or `r`-spawn) re-brings-up — no auto-retry loop,
                // which would hammer a half-open daemon with no backoff.
                LiveStep::BoxClosed => {
                    stop_forward(supervisor);
                    set_state(&notice_tx, RemoteConnState::Disconnected);
                }
                LiveStep::ModelGone => {
                    stop_forward(supervisor);
                    break;
                }
            }
        } else {
            let step = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None => IdleStep::ModelGone,
                    Some(cmd) => IdleStep::Command(cmd),
                },
                ctrl = control_rx.recv() => match ctrl {
                    None => IdleStep::ModelGone,
                    Some(RemoteControl::Connect) => IdleStep::Connect,
                    Some(RemoteControl::Disconnect) => IdleStep::Disconnect,
                },
            };
            match step {
                IdleStep::ModelGone => break,
                // Disconnect while already down — reassert the state so the
                // indicator is right even if a transient Error preceded it.
                IdleStep::Disconnect => set_state(&notice_tx, RemoteConnState::Disconnected),
                // Explicit connect: bring the box up with nothing to spawn.
                // Raced against the control channel so a `Disconnect` cancels
                // the in-flight bring-up instead of queueing behind it.
                IdleStep::Connect => {
                    let est = establish(&provider, &spec, store.as_ref(), &ports, &notice_tx);
                    match race_control(est, &mut control_rx).await {
                        Raced::Completed(Ok(up)) => connected = Some(up),
                        // `establish` already emitted the durable `error: …`.
                        Raced::Completed(Err(_)) => {}
                        Raced::Cancelled => set_state(&notice_tx, RemoteConnState::Disconnected),
                        Raced::ModelGone => break,
                    }
                }
                IdleStep::Command(cmd) => {
                    let est = establish(&provider, &spec, store.as_ref(), &ports, &notice_tx);
                    match race_control(est, &mut control_rx).await {
                        Raced::Completed(Ok((client, supervisor))) => {
                            forward(&name, &client, &notice_tx, cmd);
                            connected = Some((client, supervisor));
                        }
                        Raced::Completed(Err(e)) => {
                            // The box just proved unreachable after a full
                            // retry cycle. Everything queued behind the
                            // triggering command was riding this same
                            // bring-up — running another multi-attempt cycle
                            // per queued command would burn minutes against a
                            // dead box (a 5-row bulk fan-out ≈ 5 × ~66s of
                            // churn). Drain and drop the whole queue now, with
                            // ONE aggregate notice that rolls back every
                            // affected `⇅` tag. A command that arrives after
                            // this drain is a fresh user action and earns a
                            // fresh bring-up attempt.
                            drop_queued_commands(
                                &name,
                                &notice_tx,
                                &e,
                                drain_pending(cmd, &mut cmd_rx),
                            );
                        }
                        // The user cancelled the bring-up this spawn triggered.
                        // Drain the whole fan-out too, or the next queued spawn
                        // immediately re-triggers a bring-up the user just
                        // cancelled — then roll every affected `⇅` tag back.
                        Raced::Cancelled => {
                            set_state(&notice_tx, RemoteConnState::Disconnected);
                            drop_cancelled_commands(
                                &name,
                                &notice_tx,
                                &drain_pending(cmd, &mut cmd_rx),
                            );
                        }
                        Raced::ModelGone => break,
                    }
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

    #[test]
    fn conn_error_reason_names_the_rebuild_fix_for_a_fingerprint_mismatch() {
        // A fingerprint mismatch is a build problem, not flaky infra — the
        // persistent `error: …` indicator must name the actionable fix
        // (#977), not surface the raw handshake error.
        let handshake = lazybox_ipc::socket::HandshakeError::FingerprintMismatch {
            peer: 0x1111_1111,
            ours: 0x2222_2222,
        };
        let io = std::io::Error::other(handshake.to_string());
        let err = anyhow::Error::new(io).context("forward did not dial the box daemon");
        assert!(conn_error_reason(&err).contains("lazybox sandbox rebuild"));

        // A plain transport failure surfaces its own top-line reason.
        let other = anyhow::anyhow!("forward did not bind /tmp/x.sock within 20s");
        assert_eq!(
            conn_error_reason(&other),
            "forward did not bind /tmp/x.sock within 20s"
        );
    }

    fn spawn_cmd(key: &str) -> Command {
        Command::Spawn {
            session_key: SessionKey::from(key),
            session_id: None,
            client_request_id: None,
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            cwd: None,
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            model_alias: None,
            access: Default::default(),
        }
    }

    #[tokio::test]
    async fn drain_pending_collects_first_plus_the_whole_queue() {
        // A cancelled/failed bring-up must sweep every command riding it, or
        // the next queued spawn re-triggers a fresh bring-up (#1066).
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(spawn_cmd("github:owner/repo#2")).await.unwrap();
        tx.send(spawn_cmd("github:owner/repo#3")).await.unwrap();
        let all = drain_pending(spawn_cmd("github:owner/repo#1"), &mut rx);
        assert_eq!(all.len(), 3, "first + every queued command");
        assert!(rx.try_recv().is_err(), "the queue is fully drained");
    }

    #[test]
    fn drop_cancelled_commands_emits_one_aggregate_rollback() {
        // A cancelled bulk fan-out rolls back every `⇅` tag in ONE notice,
        // never one flash per spawn.
        let (tx, mut rx) = mpsc::channel(8);
        let cancelled = vec![
            spawn_cmd("github:owner/repo#1"),
            spawn_cmd("github:owner/repo#2"),
        ];
        drop_cancelled_commands("box", &tx, &cancelled);
        match rx.try_recv().expect("one notice") {
            RemoteBoxNotice::Dropped {
                session_keys,
                error,
            } => {
                assert_eq!(session_keys.len(), 2, "every cancelled spawn rolled back");
                assert!(error.contains("connect cancelled"), "{error}");
            }
            other => panic!("expected an aggregate Dropped, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one aggregate notice");
    }

    #[test]
    fn drop_cancelled_commands_is_silent_without_spawns() {
        // Nothing to roll back → no notice (non-spawn commands carry no tag).
        let (tx, mut rx) = mpsc::channel(8);
        drop_cancelled_commands("box", &tx, &[]);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn race_control_disconnect_cancels_an_in_flight_bring_up() {
        // The core fix (#1066): a `Disconnect` must cancel a bring-up that
        // is still in flight, not queue behind it. A never-completing future
        // stands in for a multi-minute `establish`; only the control channel
        // can end the race.
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(8);
        ctrl_tx.send(RemoteControl::Disconnect).await.unwrap();
        assert!(matches!(
            race_control(std::future::pending::<()>(), &mut ctrl_rx).await,
            Raced::Cancelled
        ));
    }

    #[tokio::test]
    async fn race_control_ignores_a_redundant_connect_then_cancels() {
        // A redundant `Connect` mid-bring-up must be ignored (dropping the
        // future would restart Terraform) — the race keeps waiting on the
        // SAME future — while a later `Disconnect` still cancels. FIFO
        // ordering makes this deterministic against a `pending` future.
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(8);
        ctrl_tx.send(RemoteControl::Connect).await.unwrap();
        ctrl_tx.send(RemoteControl::Disconnect).await.unwrap();
        assert!(matches!(
            race_control(std::future::pending::<()>(), &mut ctrl_rx).await,
            Raced::Cancelled
        ));
    }

    #[tokio::test]
    async fn race_control_completes_when_the_future_finishes_first() {
        // No control traffic → the bring-up's own result is returned.
        let (_ctrl_tx, mut ctrl_rx) = mpsc::channel::<RemoteControl>(8);
        assert!(matches!(
            race_control(async { 7 }, &mut ctrl_rx).await,
            Raced::Completed(7)
        ));
    }

    #[tokio::test]
    async fn race_control_reports_model_gone_when_control_closed() {
        // The model dropping its control sender ends the worker, even while
        // a bring-up is pending.
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<RemoteControl>(8);
        drop(ctrl_tx);
        assert!(matches!(
            race_control(std::future::pending::<()>(), &mut ctrl_rx).await,
            Raced::ModelGone
        ));
    }

    #[tokio::test]
    async fn setup_yields_a_control_channel() {
        // The worker exposes a control channel so the UI can connect /
        // disconnect on demand and auto-connect on startup (#1066).
        let sc = SandboxConfig {
            project: Some("proj".into()),
            ..SandboxConfig::default()
        };
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let rb = setup(&sc, store).expect("a configured box yields a RemoteBox");
        // The channel is open (the worker is selecting on its receiver).
        assert!(rb.control.try_send(RemoteControl::Connect).is_ok());
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
