//! In-process port-forward supervisor for `lazybox --connect`.
//!
//! The BYO-remote runbook used to tell the operator to stand up the SSH
//! forward out of process (`autossh` / `ServerAliveInterval`). This module
//! brings it in-process: it builds the forward command (`ssh -L …`, or
//! SSH-over-IAP via `gcloud compute ssh --tunnel-through-iap`), spawns it,
//! and keepalive-supervises it with capped exponential backoff so a dropped
//! link is re-established without operator intervention.
//!
//! The forward carries the daemon Unix socket — the endpoint `--connect`
//! dials — plus any workload TCP ports, all bound to `localhost` on the
//! client. A tunnel flap drops the socket too, so the visible state is the
//! transport's existing reconnect banner (`ipc::socket`): while the forward
//! is down the socket reconnect loop shows `⟳ … reconnecting…`, and the
//! moment the supervisor re-establishes the forward the socket re-dials and
//! the banner clears.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use lazybox_config::{TunnelConfig, TunnelMode};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::time::Instant;

/// Initial delay before the first re-spawn, and the value the backoff
/// resets to after a forward has proved stable. Mirrors the socket
/// reconnect supervisor so the two loops damp on the same cadence.
const BACKOFF_START: Duration = Duration::from_millis(250);
/// Ceiling on the re-spawn backoff: a box that stays unreachable is
/// retried about every five seconds rather than drifting to minute-long
/// gaps.
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A forward that stayed up at least this long counts as healthy, so the
/// next drop damps from [`BACKOFF_START`] again; a forward that dies
/// sooner is flapping (bad auth, refused port) and keeps escalating.
const STABLE: Duration = Duration::from_secs(5);

fn grow_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
}

fn next_backoff_after_exit(current: Duration, ran: Duration) -> Duration {
    if ran >= STABLE {
        BACKOFF_START
    } else {
        grow_backoff(current)
    }
}

/// A validated, ready-to-spawn forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    target: Target,
    /// Local socket the forward binds; also where `--connect` dials.
    local_socket: PathBuf,
    /// Absolute daemon-socket path on the box, when the socket is
    /// forwarded (`None` → workload ports only).
    remote_socket: Option<String>,
    ports: Vec<u16>,
    server_alive_interval: u64,
    server_alive_count_max: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Ssh {
        host: String,
    },
    Iap {
        instance: String,
        user: Option<String>,
        zone: Option<String>,
        project: Option<String>,
    },
}

/// A `remote.tunnel:` config that is missing the field its mode requires.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    MissingHost,
    MissingInstance,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            ResolveError::MissingHost => "remote.tunnel.host is required for mode: ssh",
            ResolveError::MissingInstance => "remote.tunnel.instance is required for mode: iap",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ResolveError {}

impl Tunnel {
    /// Resolve a [`TunnelConfig`] against the client's `--connect` socket
    /// path (the default local-socket endpoint when the config doesn't
    /// pin one). Fails only when a mode-required field is absent.
    pub fn resolve(
        cfg: TunnelConfig,
        connect_socket: &std::path::Path,
    ) -> Result<Self, ResolveError> {
        let target = match cfg.mode {
            TunnelMode::Ssh => Target::Ssh {
                host: cfg.host.ok_or(ResolveError::MissingHost)?,
            },
            TunnelMode::Iap => Target::Iap {
                instance: cfg.instance.ok_or(ResolveError::MissingInstance)?,
                user: cfg.user,
                zone: cfg.zone,
                project: cfg.project,
            },
        };
        Ok(Self {
            target,
            local_socket: cfg
                .local_socket
                .unwrap_or_else(|| connect_socket.to_path_buf()),
            remote_socket: cfg.remote_socket,
            ports: cfg.ports,
            server_alive_interval: cfg.server_alive_interval,
            server_alive_count_max: cfg.server_alive_count_max,
        })
    }

    /// Local socket the forward binds — the path `--connect` should dial.
    pub fn local_socket(&self) -> &std::path::Path {
        &self.local_socket
    }

    /// Whether the daemon socket (not just workload ports) is forwarded.
    /// Only then does the supervisor own — and clear stale copies of —
    /// the local socket file, and only then is [`local_socket`] a path the
    /// forward will bind (so a caller can wait for it).
    ///
    /// [`local_socket`]: Self::local_socket
    pub fn forwards_socket(&self) -> bool {
        self.remote_socket.is_some()
    }

    /// The socket a caller should wait for before dialing: the path this
    /// forward actually binds, or `None` when only ports are forwarded.
    /// Callers wait on this rather than comparing against the `--connect`
    /// path, so a socket spelled two equivalent ways (`/tmp/x` vs
    /// `/tmp/./x`) doesn't skip the readiness gate.
    pub fn readiness_path(&self) -> Option<&std::path::Path> {
        if self.forwards_socket() {
            Some(self.local_socket())
        } else {
            None
        }
    }

    /// `ssh -L` forward specs: the daemon socket first (Unix→Unix), then
    /// each workload port bound to `localhost` on both ends.
    fn forward_specs(&self) -> Vec<String> {
        let mut specs = Vec::new();
        if let Some(remote) = &self.remote_socket {
            specs.push(format!("{}:{}", self.local_socket.display(), remote));
        }
        for port in &self.ports {
            specs.push(format!("localhost:{port}:localhost:{port}"));
        }
        specs
    }

    /// Common `ssh` flags for an unattended, keepalive'd forward, followed
    /// by the `-L` specs. Shared by both modes (IAP passes them to `ssh`
    /// after `--`).
    fn ssh_forward_args(&self) -> Vec<String> {
        let mut args = vec![
            "-N".to_string(),
            "-T".to_string(),
            "-o".to_string(),
            format!("ServerAliveInterval={}", self.server_alive_interval),
            "-o".to_string(),
            format!("ServerAliveCountMax={}", self.server_alive_count_max),
            // Unattended: never block on an interactive auth prompt. ssh
            // reads passphrases / keyboard-interactive from /dev/tty, which
            // the TUI owns in raw mode — a prompt would corrupt the screen
            // and steal keystrokes, and the supervisor retries forever. Fail
            // fast instead; a working forward needs agent- or passphrase-less
            // key auth.
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            // Unattended: a first-connect host key can't be confirmed at a
            // prompt, so trust-on-first-use it rather than hang.
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
        ];
        // Deliberately no `ExitOnForwardFailure`: it applies to *every* `-L`,
        // so a workload port already taken locally (a dev server on `:3000`)
        // would sink the whole session including the daemon socket. Forwards
        // are best-effort; the daemon socket's readiness is gated separately
        // by `wait_for_socket`.
        for spec in self.forward_specs() {
            args.push("-L".to_string());
            args.push(spec);
        }
        args
    }

    /// The forward process to spawn: program + argv.
    pub fn command(&self) -> (String, Vec<String>) {
        match &self.target {
            Target::Ssh { host } => {
                let mut args = self.ssh_forward_args();
                args.push(host.clone());
                ("ssh".to_string(), args)
            }
            Target::Iap {
                instance,
                user,
                zone,
                project,
            } => {
                let dest = match user {
                    Some(u) => format!("{u}@{instance}"),
                    None => instance.clone(),
                };
                // `--quiet` so gcloud never blocks on an interactive confirm
                // (e.g. first-run SSH-key generation) — same unattended
                // stance as the inner ssh's `BatchMode`.
                let mut args = vec![
                    "compute".to_string(),
                    "ssh".to_string(),
                    dest,
                    "--quiet".to_string(),
                ];
                if let Some(zone) = zone {
                    args.push(format!("--zone={zone}"));
                }
                if let Some(project) = project {
                    args.push(format!("--project={project}"));
                }
                args.push("--tunnel-through-iap".to_string());
                args.push("--".to_string());
                args.extend(self.ssh_forward_args());
                ("gcloud".to_string(), args)
            }
        }
    }

    /// Spawn one forward process. `ssh -L` refuses to bind a Unix socket
    /// whose path already exists, so a stale copy left by a previous
    /// process (or an unclean exit) is cleared first — but only when this
    /// supervisor owns that socket, never a real local daemon's.
    fn spawn(&self) -> std::io::Result<Child> {
        if self.forwards_socket() {
            let _ = std::fs::remove_file(&self.local_socket);
        }
        let (program, args) = self.command();
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Piped, not inherited: the forward's diagnostics stay out of the
            // TUI's alternate screen but are drained into the log (see
            // `supervise_with`), so a failing tunnel — bad auth, a taken
            // workload port — is diagnosable instead of silent.
            .stderr(Stdio::piped())
            // If the supervisor task is dropped (client exit), the child
            // ssh/gcloud is killed rather than orphaned holding the ports.
            .kill_on_drop(true);
        // Own process group: `gcloud` is a Python wrapper around the real
        // `ssh -N -L`, and `kill_on_drop` only signals the wrapper — the
        // inner ssh survived it orphaned, still holding the forwarded
        // socket, so the next launch could bind-race or silently talk to
        // the dead session's forward. Same class git-ops fixed for git
        // transport helpers; the group is swept in `supervise_with`.
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()
    }
}

/// Kills the child's whole process group with SIGKILL on drop. Mirrors
/// `git-ops`'s guard: `kill_on_drop` alone reaps only the direct child,
/// and both tunnel shapes (`gcloud … --tunnel-through-iap`, provider
/// argv) wrap an inner `ssh` that must die with its wrapper.
struct KillGroupOnDrop(Option<i32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

/// Supervise the forward forever: spawn it, wait for it to exit, then
/// re-spawn with capped-backoff keepalive. Runs until the task is
/// aborted/dropped, which kills the live child (`kill_on_drop`). Live state
/// isn't published here — a forward flap drops the daemon socket with it, so
/// the transport's existing reconnect banner is the UI signal.
pub async fn supervise(tunnel: Tunnel) {
    supervise_with(|| tunnel.spawn()).await
}

/// Supervise a pre-built forward given as `program` + `args` — the shape
/// [`lazybox_sandbox::Tunnel`] hands back — reusing the same
/// capped-backoff keepalive as [`supervise`]. `socket` is the local Unix
/// socket the forward binds; a stale copy left by an earlier process is
/// cleared before each (re)spawn so `ssh -L` doesn't refuse to bind it.
/// `env` is the provider's injected credentials (#1047), overlaid on every
/// (re)spawn so the forward authenticates off configured creds rather than
/// ambient state. The `r`-spawn box lifecycle (`remote_box`) drives this:
/// `connect_box` builds the argv + env, this keeps it up for the session.
pub async fn supervise_argv(
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    socket: PathBuf,
) {
    supervise_with(move || {
        let _ = std::fs::remove_file(&socket);
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group — see `Tunnel::spawn`: the provider argv also
        // wraps an inner ssh that must die with its wrapper.
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()
    })
    .await
}

/// The supervision loop over an injectable spawner, so the backoff /
/// re-spawn machine is testable without shelling out to `ssh`/`gcloud`.
async fn supervise_with<F>(mut spawn: F)
where
    F: FnMut() -> std::io::Result<Child>,
{
    let mut backoff = BACKOFF_START;
    loop {
        match spawn() {
            Ok(mut child) => {
                // Sweep the whole group when this arm ends — whether the
                // leader exited on its own (its inner ssh may have
                // survived, orphaned on the socket) or the supervisor
                // task was aborted mid-wait. The group only contains
                // processes this spawn created.
                let _group = KillGroupOnDrop(child.id().map(|id| id as i32));
                if let Some(stderr) = child.stderr.take() {
                    tokio::spawn(log_child_stderr(stderr));
                }
                let started = Instant::now();
                let _ = child.wait().await;
                backoff = next_backoff_after_exit(backoff, started.elapsed());
                tracing::warn!("tunnel: forward exited; re-spawning in {backoff:?}");
            }
            Err(error) => {
                backoff = grow_backoff(backoff);
                tracing::warn!("tunnel: spawn failed: {error}; retrying in {backoff:?}");
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// Drain a forward's stderr into the log so a failing tunnel (auth denied,
/// a taken workload port, an unreachable host) leaves a trace instead of
/// dying silently behind the startup timeout.
async fn log_child_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::debug!("tunnel: {line}");
        }
    }
}

/// Poll for the forwarded local socket to appear, up to `timeout`. The
/// forward binds it a beat after the process starts, so `--connect` waits
/// here before dialing. Returns `false` if it never shows.
pub async fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_cfg() -> TunnelConfig {
        TunnelConfig {
            mode: TunnelMode::Ssh,
            host: Some("me@box".to_string()),
            remote_socket: Some("/home/me/.lazybox/run/daemon.sock".to_string()),
            ports: vec![3000, 8082],
            ..TunnelConfig::default()
        }
    }

    #[test]
    fn ssh_command_forwards_socket_then_localhost_ports() {
        let tunnel = Tunnel::resolve(ssh_cfg(), std::path::Path::new("/tmp/lb.sock")).unwrap();
        let (program, args) = tunnel.command();
        assert_eq!(program, "ssh");
        // Keepalive is present, and auth is non-interactive so a prompt can't
        // fight the TUI for the tty.
        assert!(args.contains(&"ServerAliveInterval=15".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        // No ExitOnForwardFailure: a taken workload port must not sink the
        // daemon-socket forward with it.
        assert!(
            !args.iter().any(|a| a.contains("ExitOnForwardFailure")),
            "workload-port failure must not be fused to the session, got: {args:?}"
        );
        // Socket forward uses the resolved local path (the --connect path).
        assert!(
            args.contains(&"/tmp/lb.sock:/home/me/.lazybox/run/daemon.sock".to_string()),
            "socket forward spec, got: {args:?}"
        );
        // Workload ports bind localhost on both ends.
        assert!(args.contains(&"localhost:3000:localhost:3000".to_string()));
        assert!(args.contains(&"localhost:8082:localhost:8082".to_string()));
        // Destination is the trailing arg.
        assert_eq!(args.last().unwrap(), "me@box");
    }

    #[test]
    fn iap_command_wraps_ssh_through_gcloud() {
        let cfg = TunnelConfig {
            mode: TunnelMode::Iap,
            instance: Some("box-1".to_string()),
            user: Some("me".to_string()),
            zone: Some("us-central1-a".to_string()),
            project: Some("proj".to_string()),
            remote_socket: Some("/home/me/.lazybox/run/daemon.sock".to_string()),
            ports: vec![3000],
            ..TunnelConfig::default()
        };
        let tunnel = Tunnel::resolve(cfg, std::path::Path::new("/tmp/lb.sock")).unwrap();
        let (program, args) = tunnel.command();
        assert_eq!(program, "gcloud");
        assert_eq!(&args[0], "compute");
        assert_eq!(&args[1], "ssh");
        assert_eq!(&args[2], "me@box-1");
        // gcloud runs non-interactively, matching the inner ssh's BatchMode.
        assert!(args.contains(&"--quiet".to_string()));
        assert!(args.contains(&"--zone=us-central1-a".to_string()));
        assert!(args.contains(&"--project=proj".to_string()));
        assert!(args.contains(&"--tunnel-through-iap".to_string()));
        // The ssh forward args follow the `--` separator.
        let sep = args.iter().position(|a| a == "--").expect("-- separator");
        let ssh_args = &args[sep + 1..];
        assert!(ssh_args.contains(&"-N".to_string()));
        assert!(ssh_args.contains(&"BatchMode=yes".to_string()));
        assert!(ssh_args.contains(&"-L".to_string()));
        assert!(ssh_args.contains(&"localhost:3000:localhost:3000".to_string()));
    }

    #[test]
    fn ports_only_tunnel_forwards_no_socket() {
        let cfg = TunnelConfig {
            mode: TunnelMode::Ssh,
            host: Some("box".to_string()),
            ports: vec![8082],
            ..TunnelConfig::default()
        };
        let tunnel = Tunnel::resolve(cfg, std::path::Path::new("/tmp/lb.sock")).unwrap();
        assert!(!tunnel.forwards_socket());
        let (_, args) = tunnel.command();
        assert!(args.iter().all(|a| !a.contains("daemon.sock")));
        assert!(args.contains(&"localhost:8082:localhost:8082".to_string()));
    }

    #[test]
    fn resolve_requires_the_mode_specific_target() {
        let bad_ssh = TunnelConfig {
            mode: TunnelMode::Ssh,
            host: None,
            ..TunnelConfig::default()
        };
        assert_eq!(
            Tunnel::resolve(bad_ssh, std::path::Path::new("/tmp/lb.sock")).unwrap_err(),
            ResolveError::MissingHost
        );
        let bad_iap = TunnelConfig {
            mode: TunnelMode::Iap,
            instance: None,
            ..TunnelConfig::default()
        };
        assert_eq!(
            Tunnel::resolve(bad_iap, std::path::Path::new("/tmp/lb.sock")).unwrap_err(),
            ResolveError::MissingInstance
        );
    }

    #[test]
    fn readiness_path_is_the_bind_path_not_the_connect_arg() {
        // A pinned local_socket spelled differently from the --connect arg
        // must still be the path we wait on — the forward binds it, so
        // gating on `local_socket == connect_arg` would wrongly skip the
        // wait. Here the pinned path differs from the connect arg entirely.
        let pinned = TunnelConfig {
            local_socket: Some(PathBuf::from("/run/pinned.sock")),
            ..ssh_cfg()
        };
        let tunnel = Tunnel::resolve(pinned, std::path::Path::new("/tmp/lb.sock")).unwrap();
        assert_eq!(
            tunnel.readiness_path(),
            Some(std::path::Path::new("/run/pinned.sock"))
        );

        // A ports-only tunnel binds no socket, so there's nothing to wait on.
        let ports_only = TunnelConfig {
            mode: TunnelMode::Ssh,
            host: Some("box".to_string()),
            ports: vec![8082],
            ..TunnelConfig::default()
        };
        let tunnel = Tunnel::resolve(ports_only, std::path::Path::new("/tmp/lb.sock")).unwrap();
        assert_eq!(tunnel.readiness_path(), None);
    }

    #[test]
    fn local_socket_defaults_to_the_connect_path_and_can_be_pinned() {
        let tunnel = Tunnel::resolve(ssh_cfg(), std::path::Path::new("/tmp/lb.sock")).unwrap();
        assert_eq!(tunnel.local_socket(), std::path::Path::new("/tmp/lb.sock"));

        let pinned = TunnelConfig {
            local_socket: Some(PathBuf::from("/run/pinned.sock")),
            ..ssh_cfg()
        };
        let tunnel = Tunnel::resolve(pinned, std::path::Path::new("/tmp/lb.sock")).unwrap();
        assert_eq!(
            tunnel.local_socket(),
            std::path::Path::new("/run/pinned.sock")
        );
    }

    #[test]
    fn backoff_grows_to_cap_then_resets_after_a_stable_run() {
        let mut b = BACKOFF_START;
        for _ in 0..10 {
            b = grow_backoff(b);
        }
        assert_eq!(b, BACKOFF_MAX, "backoff caps");
        // A flapping (short-lived) forward keeps escalating…
        assert_eq!(
            next_backoff_after_exit(BACKOFF_START, Duration::from_millis(100)),
            grow_backoff(BACKOFF_START)
        );
        // …a forward that proved stable resets to the floor.
        assert_eq!(next_backoff_after_exit(BACKOFF_MAX, STABLE), BACKOFF_START);
    }

    #[tokio::test]
    async fn supervise_respawns_after_each_forward_exits() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A stand-in "forward" that exits immediately, counting spawns.
        // The loop must re-spawn it, so the count keeps climbing.
        let spawns = Arc::new(AtomicUsize::new(0));
        let spawns_in = spawns.clone();
        let spawn = move || {
            spawns_in.fetch_add(1, Ordering::SeqCst);
            Command::new("true").kill_on_drop(true).spawn()
        };
        let handle = tokio::spawn(supervise_with(spawn));

        // Each attempt spends the ~250ms floor backing off, so a handful of
        // re-spawns fit comfortably in this window.
        let respawned = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if spawns.load(Ordering::SeqCst) >= 3 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false);
        handle.abort();
        assert!(
            respawned,
            "supervisor must re-spawn the forward after it exits"
        );
    }

    #[tokio::test]
    async fn supervise_recovers_from_a_spawn_failure() {
        // A spawner that errors on the first attempt then succeeds proves the
        // error arm also backs off and retries rather than giving up: the
        // loop must reach a second (successful) attempt.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_in = attempts.clone();
        let spawn = move || {
            let n = attempts_in.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no binary",
                ))
            } else {
                Command::new("true").kill_on_drop(true).spawn()
            }
        };
        let handle = tokio::spawn(supervise_with(spawn));
        let recovered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if attempts.load(Ordering::SeqCst) >= 2 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false);
        handle.abort();
        assert!(recovered, "a spawn failure must retry, not abort the loop");
    }

    #[tokio::test]
    async fn wait_for_socket_sees_a_late_bind() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("lb.sock");
        let sock_in = sock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            std::fs::write(&sock_in, b"").unwrap();
        });
        assert!(wait_for_socket(&sock, Duration::from_secs(2)).await);

        let missing = dir.path().join("never.sock");
        assert!(!wait_for_socket(&missing, Duration::from_millis(200)).await);
    }
}
