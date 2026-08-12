//! `lazybox sandbox <ensure|wake|sleep|status|connect|rebuild|destroy>` —
//! the client surface for the remote dev-box lifecycle (#931).
//!
//! `ensure`/`destroy` drive the Terraform module; `wake`/`sleep`/`status`/
//! `connect`/`rebuild` use the native `gcloud`/IAP path. `ensure` provisions
//! a box that builds + runs a wire-compatible lazybox daemon on boot (#977),
//! and `rebuild` re-syncs that daemon to the client's commit without a reboot.
//!
//! A [`BoxHandle`] is persisted
//! in the store under the **shared box key** — one `sandbox:` block, one
//! box, the same identity the TUI's `r`-spawn targets (`remote_box`) — so
//! `wake`/`sleep`/`connect` address the box `ensure` stamped without
//! re-running Terraform. `--worktree <key>` opts a command into a separate
//! per-key box (the pre-#965 per-worktree model). Configuration comes from
//! the `sandbox:` block (see [`SandboxConfig`]); every field is overridable
//! per-command by a flag.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use lazybox_config::{Config, SandboxConfig};
use lazybox_sandbox::gcp::{GcpAuth, GcpProvider, SystemRunner};
use lazybox_sandbox::{
    BoxHandle, Deployment, PowerState, SandboxProvider, SandboxSpec, connect_box, persist,
};
use lazybox_store::Store;
use tokio::process::Command;

use crate::take_value;

/// Local socket the connect forward binds when nothing else is configured
/// — the same default `contrib/box-lifecycle/connect.sh` uses.
const DEFAULT_LOCAL_SOCKET: &str = "/tmp/lazybox.sock";

/// The single shared box's stable identity — the store/tfstate key both
/// this CLI **and** the TUI's `r`-spawn worker (`remote_box`) resolve by
/// default, so the two can never silently provision two different boxes
/// off one `sandbox:` block (#965).
pub(crate) const SHARED_BOX_KEY: &str = "sandbox";

/// The box's daemon socket, **relative** to the SSH login home: `ssh -L`
/// resolves a relative remote socket against the box user's `$HOME`, so
/// lazybox forwards to it without knowing that path. Matches
/// `contrib/box-lifecycle/connect.sh` (`LAZYBOX_BOX_SOCK` default). The
/// convention applies whenever `sandbox.remote_socket` is unset — the
/// product path sets nothing.
pub(crate) const BOX_DAEMON_SOCKET: &str = ".lazybox/run/daemon.sock";

/// The dedicated login/daemon user provisioning creates when it installs
/// lazybox (#977). The daemon runs as this account, so its socket lands at
/// `~lazybox/.lazybox/run/daemon.sock` — the client forwards the relative
/// [`BOX_DAEMON_SOCKET`] against this user's home, so `connect`/`r`-spawn
/// hit it with zero socket config as long as they SSH in as this user.
pub(crate) const BOX_LAZYBOX_USER: &str = "lazybox";

/// Whether `ensure` provisions a box that builds + runs the lazybox daemon
/// (#977). Config-only (not a flag) so both [`resolve_provider`] and
/// [`resolve_spec`] can read it off one `sandbox:` block without racing to
/// consume the same arg. Unset defaults to `true` — the product path wants
/// a wire-compatible daemon by construction.
pub(crate) fn install_lazybox_enabled(sc: &SandboxConfig) -> bool {
    sc.install_lazybox.unwrap_or(true)
}

/// The client's own build commit — the value baked into this binary and
/// printed by `lazybox --version`. The box's daemon is built from it so the
/// wire fingerprint matches by construction. A build outside a git checkout
/// bakes `"unknown"`; map that to empty so the box tracks the default branch
/// tip rather than trying to `git checkout unknown`.
pub(crate) fn client_build_sha() -> String {
    normalize_build_sha(lazybox_ipc::BUILD_GIT_SHA)
}

/// Map a baked build SHA to the value passed to Terraform: a real commit
/// passes through; the `"unknown"` sentinel a non-git build bakes (and an
/// empty string) become empty, so the box tracks the default branch tip
/// instead of trying to `git checkout unknown`.
fn normalize_build_sha(sha: &str) -> String {
    if sha.is_empty() || sha == "unknown" {
        String::new()
    } else {
        sha.to_string()
    }
}

/// A warning when this client is a **dirty** build (uncommitted changes).
/// [`client_build_sha`] resolves to the clean commit, so the box builds *that*
/// — meaning uncommitted wire changes (`crates/ipc`/`core`) leave the
/// fingerprints mismatched right after `ensure`/`rebuild`, and no rebuild can
/// close the gap (it rebuilds the same clean commit); only committing can.
/// Stripping `-dirty` silently is how that reads as a baffling "mismatch
/// immediately after ensure", so say it up front. `None` for a clean build.
/// Takes the version string so both branches are unit-tested.
fn dirty_build_warning(build_version: &str) -> Option<String> {
    build_version.contains("-dirty").then(|| {
        "warning: this lazybox client is a dirty build (uncommitted changes); the box \
         builds the clean commit, so any wire changes you haven't committed won't match \
         — commit them for guaranteed build parity"
            .to_string()
    })
}

pub async fn sandbox_subcommand(args: &[String]) -> anyhow::Result<()> {
    // `init_tracing` redirects process stderr into the log file before any
    // subcommand runs, so a returned error (terraform/gcloud failures land
    // here) would vanish from the terminal. Echo it on stdout, which is not
    // redirected, so a failed `lazybox sandbox …` says why.
    let result = dispatch(args).await;
    if let Err(err) = &result {
        println!("sandbox: {err:#}");
    }
    result
}

async fn dispatch(args: &[String]) -> anyhow::Result<()> {
    let (verb, rest) = args
        .split_first()
        .map(|(v, r)| (v.as_str(), r))
        .unwrap_or(("", &[]));
    let mut rest = rest.to_vec();
    match verb {
        "ensure" => ensure(&mut rest).await,
        "wake" | "start" => wake(&mut rest).await,
        "sleep" | "stop" => sleep(&mut rest).await,
        "status" => status(&mut rest).await,
        "connect" => connect(&mut rest).await,
        "rebuild" => rebuild(&mut rest).await,
        "destroy" => destroy(&mut rest).await,
        other => anyhow::bail!(
            "unknown `lazybox sandbox` verb {other:?}; usage: lazybox sandbox \
             <ensure|wake|sleep|status|connect|rebuild|destroy> [--worktree <key>] [flags]"
        ),
    }
}

/// The key a handle is stored under: `--worktree <key>` for an explicit
/// per-key box, else the [`SHARED_BOX_KEY`]. Pre-#965 this defaulted to
/// the git worktree root — a per-worktree box the TUI's `r`-spawn could
/// never find (it has no worktree). One `sandbox:` block now means one
/// box, addressed identically from the CLI and the TUI; a stale
/// worktree-keyed handle is reachable by passing that path via
/// `--worktree`.
fn box_key(args: &mut Vec<String>) -> String {
    take_value(args, "--worktree").unwrap_or_else(|| SHARED_BOX_KEY.to_string())
}

/// A stable 8-hex-digit digest of `s`, used to disambiguate box identities
/// that would otherwise collide on a shared basename. Computed once at
/// `ensure` and then carried in the persisted handle, so cross-release
/// hasher drift never matters (later ops read `handle.id`, they don't
/// recompute it).
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// Derive the GCE instance name: `--name` verbatim (already sanitized by
/// the caller), else `lazybox-sbx-<slug>-<hash>` built from the worktree
/// key. The `lazybox-sbx-` prefix guarantees the leading-letter + lowercase
/// GCE naming rule regardless of the worktree path, and the trailing hash
/// keeps two worktrees that share a basename (e.g. `issue-931` in two repos)
/// from colliding on one box.
fn instance_name(explicit: Option<String>, worktree: &str) -> String {
    if let Some(name) = explicit
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
    {
        return name;
    }
    let base = worktree
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(worktree);
    let slug = lazybox_core::slug::slugify(base);
    let slug = if slug.is_empty() {
        "box".to_string()
    } else {
        slug
    };
    // GCE caps instance names at 63 chars: 12 (prefix) + slug + 1 (`-`) + 8
    // (hash) must fit, so bound the slug to 36.
    let slug: String = slug.chars().take(36).collect();
    format!("lazybox-sbx-{slug}-{}", short_hash(worktree))
}

/// What the derived instance name identifies. A per-`--worktree` box uses
/// the worktree key itself; the shared box salts in the local username
/// **and machine name** so neither two people sharing a GCP project nor
/// one person on two machines collide on one GCE instance name. The
/// machine component matters because tfstate is machine-local: the same
/// user's second laptop can never *adopt* the first's box, so without it
/// `ensure` there would try to create a second instance under the same
/// name and terraform would error out. Local state (store handle,
/// tfstate) still keys off the plain [`SHARED_BOX_KEY`] — the salt only
/// disambiguates the cloud-side name.
fn name_salt(key: &str) -> String {
    if key != SHARED_BOX_KEY {
        return key.to_string();
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let mut salt = SHARED_BOX_KEY.to_string();
    for part in [
        lazybox_core::slug::slugify(&user),
        lazybox_core::slug::slugify(&machine_name()),
    ] {
        if !part.is_empty() {
            salt.push('-');
            salt.push_str(&part);
        }
    }
    salt
}

/// Best-effort machine name for the shared box's cloud-side identity:
/// `$HOSTNAME` when exported, else `uname -n`, else empty (the salt
/// degrades to user-only). Never an error — a machine we can't name
/// still gets a working, merely less-unique, instance name.
fn machine_name() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return h.trim().to_string();
    }
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// This box's isolated Terraform state file, under
/// `~/.lazybox/v2/sandbox/<hash>/terraform.tfstate` — one per worktree key,
/// out of the shared module source tree, so two worktrees never share
/// state.
fn state_file_for(worktree: &str) -> PathBuf {
    lazybox_core::paths::state_root()
        .join("sandbox")
        .join(short_hash(worktree))
        .join("terraform.tfstate")
}

/// Parse `--ports 3000,8082`, falling back to the configured ports.
fn resolve_ports(raw: Option<String>, cfg: &[u16]) -> anyhow::Result<Vec<u16>> {
    let Some(raw) = raw else {
        return Ok(cfg.to_vec());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| anyhow::anyhow!("invalid port {s:?} in --ports"))
        })
        .collect()
}

/// Build the provider from config + flags, with this worktree's isolated
/// Terraform state. Only `gcp` is implemented.
pub(crate) fn resolve_provider(
    sc: &SandboxConfig,
    args: &mut Vec<String>,
    worktree: &str,
) -> anyhow::Result<GcpProvider> {
    let provider = take_value(args, "--provider")
        .or_else(|| sc.provider.clone())
        .unwrap_or_else(|| "gcp".to_string());
    if provider != "gcp" {
        anyhow::bail!("sandbox provider {provider:?} is not implemented (only `gcp` today)");
    }
    let terraform_dir = take_value(args, "--terraform-dir")
        .map(PathBuf::from)
        .or_else(|| sc.terraform_dir.clone())
        .unwrap_or_else(|| PathBuf::from("terraform/sandbox/gcp"));
    // Default the SSH login to the dedicated `lazybox` user provisioning
    // creates when it installs the daemon (#977): its socket lives under
    // that user's home, so `connect`/`r`-spawn must SSH in as it to reach
    // the conventional daemon socket. An explicit `--user`/`sandbox.user`
    // wins; a bring-your-own-stack box (install_lazybox=false) keeps
    // gcloud's default user.
    let user = take_value(args, "--user")
        .or_else(|| sc.user.clone())
        .or_else(|| install_lazybox_enabled(sc).then(|| BOX_LAZYBOX_USER.to_string()));
    let remote_socket = take_value(args, "--remote-socket").or_else(|| sc.remote_socket.clone());
    let local_socket = take_value(args, "--local-socket")
        .map(PathBuf::from)
        .or_else(|| sc.local_socket.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCAL_SOCKET));
    Ok(GcpProvider {
        terraform_dir,
        state_file: state_file_for(worktree),
        user,
        // Unset → the conventional home-relative box daemon socket, the
        // same one the box's systemd/tmux daemon binds — so `connect`
        // works with zero socket config, exactly like the `r`-spawn.
        remote_socket: remote_socket.unwrap_or_else(|| BOX_DAEMON_SOCKET.to_string()),
        local_socket,
        runner: Arc::new(SystemRunner),
        auth: resolve_auth(sc, args),
    })
}

/// The lazybox-owned `CLOUDSDK_CONFIG` dir credentials activate into, so
/// lazybox's gcloud state is isolated from the user's own `~/.config/gcloud`
/// (#1047). Under the state root, next to the per-box tfstate.
fn default_gcloud_config_dir() -> PathBuf {
    lazybox_core::paths::state_root()
        .join("sandbox")
        .join("gcloud")
}

/// Build the provider's credentials from config + flags. Flags override the
/// `sandbox.auth` block. The scoped config dir defaults to a lazybox-owned
/// directory only when a `service_account_key` is the base credential it
/// isolates — impersonation whose base is ambient/metadata must NOT scope
/// gcloud away from that base, and the ambient path keeps the user's own
/// config untouched.
pub(crate) fn resolve_auth(sc: &SandboxConfig, args: &mut Vec<String>) -> GcpAuth {
    let service_account_key = take_value(args, "--service-account-key")
        .map(PathBuf::from)
        .or_else(|| sc.auth.service_account_key.clone());
    let impersonate_service_account = take_value(args, "--impersonate-service-account")
        .or_else(|| sc.auth.impersonate_service_account.clone());
    let config_dir = take_value(args, "--gcloud-config-dir")
        .map(PathBuf::from)
        .or_else(|| sc.auth.config_dir.clone())
        .or_else(|| {
            service_account_key
                .is_some()
                .then(default_gcloud_config_dir)
        });
    GcpAuth {
        service_account_key,
        impersonate_service_account,
        config_dir,
    }
}

/// Build the full spec for `ensure` from config + flags.
pub(crate) fn resolve_spec(
    sc: &SandboxConfig,
    args: &mut Vec<String>,
    worktree: &str,
) -> anyhow::Result<SandboxSpec> {
    let deployment = resolve_deployment(sc, args)?;
    resolve_spec_with_deployment(sc, args, worktree, deployment)
}

/// Resolve the deployment recipe (`--deployment` flag → config overlay →
/// embedded default) on its own, so a caller that only needs the
/// deployment's declared `workload_ports` (e.g. `connect` to an
/// already-stamped box) doesn't have to satisfy the full spec's
/// `project` requirement.
pub(crate) fn resolve_deployment(
    sc: &SandboxConfig,
    args: &mut Vec<String>,
) -> anyhow::Result<Deployment> {
    let overlay_path = take_value(args, "--deployment")
        .map(PathBuf::from)
        .or_else(|| sc.deployment.clone());
    match overlay_path {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("read deployment overlay {}: {e}", path.display()))?;
            Ok(Deployment::with_overlay(&text)?)
        }
        None => Ok(Deployment::default_recipe()?),
    }
}

fn resolve_spec_with_deployment(
    sc: &SandboxConfig,
    args: &mut Vec<String>,
    worktree: &str,
    deployment: Deployment,
) -> anyhow::Result<SandboxSpec> {
    let project = take_value(args, "--project")
        .or_else(|| sc.project.clone())
        .ok_or_else(|| anyhow::anyhow!("no project: set sandbox.project or pass --project"))?;
    let region = take_value(args, "--region")
        .or_else(|| sc.region.clone())
        .unwrap_or_else(|| "us-central1".to_string());
    let zone = take_value(args, "--zone")
        .or_else(|| sc.zone.clone())
        .unwrap_or_else(|| "us-central1-a".to_string());
    let name = instance_name(take_value(args, "--name"), &name_salt(worktree));
    Ok(SandboxSpec {
        provider: "gcp".to_string(),
        name,
        project,
        region,
        zone,
        deployment,
        // Build parity by construction (#977): install a daemon (unless the
        // deployment opts out) built from this client's own commit.
        install_lazybox: install_lazybox_enabled(sc),
        lazybox_git_sha: client_build_sha(),
    })
}

/// `base` plus every port in `extra` not already present — the forward
/// set both `connect` and the r-spawn derive from `sandbox.ports` and
/// the deployment's `workload_ports`, so neither path silently ignores
/// a port the other honors.
pub(crate) fn union_ports(mut base: Vec<u16>, extra: &[u16]) -> Vec<u16> {
    for p in extra {
        if !base.contains(p) {
            base.push(*p);
        }
    }
    base
}

fn open_store() -> anyhow::Result<std::sync::Arc<dyn Store>> {
    lazybox_server::open_store().map_err(|e| anyhow::anyhow!("open store: {e}"))
}

fn load_handle_or_bail(store: &dyn Store, worktree: &str) -> anyhow::Result<BoxHandle> {
    match persist::load_handle(store, worktree)? {
        Some(handle) => Ok(handle),
        None => anyhow::bail!(missing_handle_message(store, worktree)),
    }
}

/// The "no box stamped" error, naming any handle stamped under a
/// *different* key. Pre-#965 builds keyed boxes per git worktree; after
/// the shared-key unification those instances still exist (and bill)
/// but the default lookup can't see them — silence here is how a user
/// ends up paying for an orphaned box, so every miss lists what IS
/// stamped and how to address it.
fn missing_handle_message(store: &dyn Store, worktree: &str) -> String {
    let others: Vec<String> = persist::list_handle_keys(store)
        .unwrap_or_default()
        .into_iter()
        .filter(|k| k != worktree)
        .collect();
    if others.is_empty() {
        format!(
            "no box stamped for {worktree:?}; run `lazybox sandbox ensure` first \
             (or pass --worktree)"
        )
    } else {
        format!(
            "no box stamped for {worktree:?}, but existing box handle(s) found under: \
             {} — address one with `--worktree <key>` (earlier builds keyed boxes per \
             git worktree; those instances keep running and billing until slept or \
             destroyed)",
            others
                .iter()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

async fn ensure(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    let spec = resolve_spec(&config.sandbox, args, &worktree)?;
    let store = open_store()?;
    // Preflight the provider's credentials so a missing/expired credential
    // fails here with a fix hint, not minutes into a terraform apply (#1047).
    provider.check_auth().await?;

    // Terraform's `-state=<path>` writes the file but not its parent dir.
    if let Some(parent) = provider.state_file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create sandbox state dir {}: {e}", parent.display()))?;
    }
    // A dirty client can't get a fingerprint-matching daemon (the box builds
    // the clean commit); warn before spending minutes on a box that will still
    // mismatch. Only when we're actually installing the daemon.
    if spec.install_lazybox
        && let Some(w) = dirty_build_warning(lazybox_ipc::BUILD_VERSION)
    {
        println!("{w}");
    }
    println!("Provisioning box {} (terraform apply)…", spec.name);
    let handle = provider.ensure(&spec).await?;
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!(
        "Box ready: {} in {} ({}). Wake/sleep/connect with `lazybox sandbox …`.",
        handle.id, handle.zone, handle.power_state
    );
    Ok(())
}

async fn wake(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;
    provider.check_auth().await?;

    println!("Waking {}…", handle.id);
    provider.start(&handle).await?;
    handle.observe(PowerState::Running, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!("{} is running.", handle.id);
    Ok(())
}

async fn sleep(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;
    provider.check_auth().await?;

    println!("Sleeping {}…", handle.id);
    provider.stop(&handle).await?;
    handle.observe(PowerState::Stopped, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!("{} is stopped (costs nothing while asleep).", handle.id);
    Ok(())
}

async fn status(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;
    provider.check_auth().await?;

    let status = provider.status(&handle).await?;
    handle.observe(status.power, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!(
        "{}: {} ({})",
        handle.id,
        status.power,
        if status.reachable {
            "reachable"
        } else {
            "not reachable"
        }
    );
    Ok(())
}

async fn connect(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    let print_only = crate::take_flag(args, "--print");
    // Provisioning is opt-in for `connect`: creating cloud infrastructure
    // costs real money, and a missing handle is at least as likely to mean
    // "wrong key" (e.g. a legacy per-worktree box) as "no box yet". The
    // TUI's `r`-spawn keeps its automatic ensure — that's the deliberate
    // invisible-lifecycle product path (#965); a CLI verb named `connect`
    // is not where a terraform apply should be a surprise.
    let provision = crate::take_flag(args, "--provision");
    // Forward ports: an explicit `--ports` is honored verbatim (the user
    // asked for exactly that set); otherwise `sandbox.ports` is unioned
    // with the deployment's declared `workload_ports` — the same set the
    // r-spawn forwards, so the two paths can't silently diverge. An
    // unreadable deployment recipe only degrades the port set on the
    // stamped-box path (warned); provisioning still hard-fails on it.
    let explicit_ports = take_value(args, "--ports");
    let deployment = resolve_deployment(&config.sandbox, args);
    let ports = match explicit_ports {
        Some(raw) => resolve_ports(Some(raw), &[])?,
        None => {
            let extra = match &deployment {
                Ok(d) => d.config.workload_ports.clone(),
                Err(e) => {
                    println!(
                        "warning: deployment recipe unreadable ({e:#}); forwarding \
                         configured ports only"
                    );
                    Vec::new()
                }
            };
            union_ports(config.sandbox.ports.clone(), &extra)
        }
    };
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    if provider.remote_socket.is_empty() {
        anyhow::bail!(
            "no remote socket: set sandbox.remote_socket or pass --remote-socket \
             (the absolute daemon-socket path on the box)"
        );
    }
    provider.check_auth().await?;
    let store = open_store()?;

    let existing = persist::load_handle(store.as_ref(), &worktree)?;
    let (mut handle, tunnel) = match existing {
        // Stamped box: wake-if-stopped + tunnel. No spec needed, so a
        // config that dropped `project` after `ensure` still connects.
        Some(handle) => {
            let tunnel = provider.connect(&handle, &ports).await?;
            (handle, tunnel)
        }
        None if provision => {
            // The same ensure → connect sequence the TUI's `r`-spawn
            // runs — one engine, two callers (#965).
            let spec = resolve_spec_with_deployment(&config.sandbox, args, &worktree, deployment?)?;
            if let Some(parent) = provider.state_file.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!("create sandbox state dir {}: {e}", parent.display())
                })?;
            }
            println!("Provisioning {} (terraform apply)…", spec.name);
            connect_box(&provider, &spec, None, &ports).await?
        }
        None => anyhow::bail!(
            "{} — or rerun with --provision to create it now",
            missing_handle_message(store.as_ref(), &worktree)
        ),
    };
    handle.observe(PowerState::Running, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;

    // Prefix the injected credential env (#1047) so a copy-pasted `--print`
    // command runs the same authenticated forward this process would spawn —
    // without it, the printed line works only on an ambient-auth machine.
    let shown = tunnel
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .chain(std::iter::once(tunnel.program.clone()))
        .chain(tunnel.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    if print_only {
        println!("{shown}");
        return Ok(());
    }

    println!("Opening tunnel — socket {}", tunnel.local_socket.display());
    println!(
        "In another shell: lazybox --connect {}",
        tunnel.local_socket.display()
    );
    // Run the forward in the foreground; Ctrl-C tears it (and the child) down.
    // The credential env rides the spawn so the IAP SSH authenticates off the
    // configured creds, not ambient state (#1047).
    let status = Command::new(&tunnel.program)
        .args(&tunnel.args)
        .envs(tunnel.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `{}`: {e}", tunnel.program))?;
    if !status.success() {
        anyhow::bail!("tunnel `{}` exited with {status}", tunnel.program);
    }
    Ok(())
}

/// How long a rebuild waits for a freshly-woken box to accept SSH before
/// giving up, and how often it re-probes. A `start` acks the GCE API well
/// before sshd is up, so the rebuild's SSH would fail in that window without
/// this — the same wake→sshd poll `contrib/box-lifecycle/connect.sh` does.
const REBUILD_SSH_TIMEOUT: Duration = Duration::from_secs(120);
const REBUILD_SSH_POLL: Duration = Duration::from_secs(5);

/// Poll `provider.status` until an IAP SSH actually completes (sshd accepting)
/// or `timeout` elapses. A transient status error (the box still booting)
/// counts as not-yet-reachable and keeps polling, so only a real timeout
/// bails. Generic over the provider so the wake→sshd wait is unit-tested
/// without a live box.
async fn wait_until_reachable<P: SandboxProvider>(
    provider: &P,
    handle: &BoxHandle,
    timeout: Duration,
    poll: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if provider
            .status(handle)
            .await
            .map(|s| s.reachable)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "box {} did not accept SSH within {}s of waking",
                handle.id,
                timeout.as_secs()
            );
        }
        tokio::time::sleep(poll).await;
    }
}

/// `lazybox sandbox rebuild [--sha <commit>]` — rebuild the box's daemon at
/// this client's commit and restart it, without a reboot. The recovery path
/// for a wire-fingerprint mismatch (#977): changing GCE metadata does not
/// re-run the startup script on a live box, so the parity fix runs the on-box
/// `lazybox-build.sh` over IAP SSH. Defaults to the client's own baked SHA
/// (so the daemon matches this binary); `--sha` targets a specific commit.
async fn rebuild(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    // `take_value` consumes the flag, so read it once. No `--sha` means rebuild
    // to this client's own commit — where a dirty build can't clear a mismatch
    // caused by uncommitted wire changes (it rebuilds the clean commit), so warn.
    let explicit_sha = take_value(args, "--sha");
    if explicit_sha.is_none()
        && let Some(w) = dirty_build_warning(lazybox_ipc::BUILD_VERSION)
    {
        println!("{w}");
    }
    let sha = explicit_sha.unwrap_or_else(client_build_sha);
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;
    provider.check_auth().await?;

    // The rebuild runs over SSH, so the box must be awake AND accepting SSH.
    if !provider.status(&handle).await?.power.is_running() {
        println!("Waking {} before rebuild…", handle.id);
        provider.start(&handle).await?;
        handle.observe(PowerState::Running, chrono::Utc::now());
        persist::save_handle(store.as_ref(), &worktree, &handle)?;
        // `start` only acks the GCE API; sshd isn't up yet. Wait for it, or the
        // rebuild ssh fails in the wake→sshd window.
        println!("Waiting for {} to accept SSH…", handle.id);
        wait_until_reachable(&provider, &handle, REBUILD_SSH_TIMEOUT, REBUILD_SSH_POLL).await?;
    }

    let target = if sha.is_empty() {
        "the default branch tip".to_string()
    } else {
        sha.clone()
    };
    println!(
        "Rebuilding lazybox on {} at {target} (this can take 10+ minutes)…",
        handle.id
    );
    let (program, cmd_args) = provider.rebuild_command(&handle, &sha);
    // Foreground with inherited stdout/stderr so the build streams; only
    // stdin is closed (the box build is non-interactive). The rebuild's IAP
    // SSH is spawned here, outside the `CommandRunner`, so it carries the
    // injected credentials too (#1047).
    let status = Command::new(&program)
        .args(&cmd_args)
        .envs(provider.auth.command_env().iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `{program}`: {e}"))?;
    if !status.success() {
        anyhow::bail!("rebuild `{program}` exited with {status}");
    }
    println!("{} rebuilt and daemon restarted.", handle.id);
    Ok(())
}

async fn destroy(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = box_key(args);
    // Destroying a box is irreversible, so it is gated behind an explicit
    // opt-in rather than run on a bare `destroy`.
    if !crate::take_flag(args, "--yes") {
        anyhow::bail!(
            "refusing to destroy without --yes (this tears the box down via `terraform destroy`)"
        );
    }
    let provider = resolve_provider(&config.sandbox, args, &worktree)?;
    let store = open_store()?;
    let handle = load_handle_or_bail(store.as_ref(), &worktree)?;
    provider.check_auth().await?;

    println!("Destroying {} (terraform destroy)…", handle.id);
    provider.destroy(&handle).await?;
    persist::delete_handle(store.as_ref(), &worktree)?;
    println!("{} destroyed.", handle.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_handle_message_names_legacy_handles() {
        // A pre-#965 build stamped the box under a git-worktree path; the
        // shared-key miss must surface it (that instance keeps billing)
        // instead of steering straight to a duplicate `ensure`.
        let store = lazybox_store::MemoryStore::new();
        let plain = missing_handle_message(&store, SHARED_BOX_KEY);
        assert!(plain.contains("ensure"), "{plain}");
        assert!(!plain.contains("existing box handle"), "{plain}");

        let legacy = lazybox_sandbox::BoxHandle {
            provider: "gcp".into(),
            id: "lazybox-sbx-old".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            project: "proj".into(),
            power_state: lazybox_sandbox::PowerState::Running,
            last_active: None,
        };
        persist::save_handle(&store, "/repos/foo/wt", &legacy).unwrap();
        let hinted = missing_handle_message(&store, SHARED_BOX_KEY);
        assert!(hinted.contains("/repos/foo/wt"), "{hinted}");
        assert!(hinted.contains("--worktree"), "{hinted}");
    }

    #[test]
    fn box_key_defaults_to_the_shared_key_and_honors_worktree() {
        // One `sandbox:` block = one box: the CLI and the TUI r-spawn
        // must resolve the same identity (#965); `--worktree` opts into
        // a separate per-key box.
        let mut args = vec![];
        assert_eq!(box_key(&mut args), SHARED_BOX_KEY);
        let mut args = vec!["--worktree".to_string(), "/repos/foo/wt".to_string()];
        assert_eq!(box_key(&mut args), "/repos/foo/wt");
    }

    #[test]
    fn name_salt_disambiguates_only_the_shared_key() {
        // Per-worktree identities pass through untouched; the shared key
        // picks up per-user AND per-machine suffixes: two people sharing
        // one GCP project must not collide on a single instance name, and
        // one person's second machine (whose local tfstate can't adopt
        // the first box) must not collide with their first.
        assert_eq!(name_salt("/repos/foo/wt"), "/repos/foo/wt");
        let salted = name_salt(SHARED_BOX_KEY);
        assert!(salted.starts_with(SHARED_BOX_KEY), "{salted}");
        if std::env::var("USER").is_ok_and(|u| !lazybox_core::slug::slugify(&u).is_empty()) {
            assert_ne!(
                salted, SHARED_BOX_KEY,
                "a resolvable user must salt the name"
            );
        }
        let machine = lazybox_core::slug::slugify(&machine_name());
        if !machine.is_empty() {
            assert!(
                salted.contains(&machine),
                "a resolvable machine name must salt the name: {salted}"
            );
        }
    }

    #[test]
    fn instance_name_prefers_explicit_then_slugs_the_worktree_basename() {
        assert_eq!(
            instance_name(Some("my-box".into()), "/home/me/wt"),
            "my-box"
        );
        // Basename slug plus a stable hash suffix of the full key.
        let name = instance_name(None, "/home/me/worktrees/Issue 931 Feature/");
        assert!(name.starts_with("lazybox-sbx-issue-931-feature-"), "{name}");
        // A pathological worktree still yields a GCE-legal name.
        assert!(instance_name(None, "///").starts_with("lazybox-sbx-box-"));
    }

    #[test]
    fn instance_name_disambiguates_same_basename_across_worktrees() {
        // Two worktrees named `issue-931` in different repos must not
        // collide on one box.
        let a = instance_name(None, "/repos/foo/issue-931");
        let b = instance_name(None, "/repos/bar/issue-931");
        assert_ne!(a, b, "same basename, different key → different box");
    }

    #[test]
    fn instance_name_is_bounded_for_gce() {
        let long = "a".repeat(200);
        let name = instance_name(None, &long);
        assert!(name.starts_with("lazybox-sbx-"));
        assert!(
            name.len() <= 63,
            "GCE caps instance names at 63: {}",
            name.len()
        );
    }

    #[test]
    fn resolve_ports_flag_overrides_config_else_falls_back() {
        assert_eq!(
            resolve_ports(Some("3000, 8082".into()), &[22]).unwrap(),
            vec![3000, 8082]
        );
        assert_eq!(resolve_ports(None, &[22, 80]).unwrap(), vec![22, 80]);
        assert!(resolve_ports(Some("nope".into()), &[]).is_err());
    }

    #[test]
    fn resolve_provider_rejects_non_gcp() {
        let sc = SandboxConfig {
            provider: Some("azure".into()),
            ..SandboxConfig::default()
        };
        let mut args = vec![];
        assert!(resolve_provider(&sc, &mut args, "/wt").is_err());
    }

    #[test]
    fn resolve_provider_takes_flags_over_config() {
        let sc = SandboxConfig {
            terraform_dir: Some(PathBuf::from("/cfg/tf")),
            remote_socket: Some("/cfg.sock".into()),
            ..SandboxConfig::default()
        };
        let mut args = vec![
            "--terraform-dir".into(),
            "/flag/tf".into(),
            "--user".into(),
            "dev".into(),
        ];
        let p = resolve_provider(&sc, &mut args, "/home/me/wt").unwrap();
        assert_eq!(p.terraform_dir, PathBuf::from("/flag/tf"));
        assert_eq!(p.user.as_deref(), Some("dev"));
        // Config fills what the flags did not.
        assert_eq!(p.remote_socket, "/cfg.sock");
        // State is isolated per worktree, under the lazybox state root.
        assert!(
            p.state_file.ends_with("terraform.tfstate"),
            "{:?}",
            p.state_file
        );
        assert!(p.state_file.starts_with(lazybox_core::paths::state_root()));
    }

    #[test]
    fn resolve_auth_is_ambient_by_default_and_scopes_when_configured() {
        // Nothing configured → ambient: no scoped config dir (gcloud stays on
        // the user's own config), the legacy path.
        let ambient = resolve_auth(&SandboxConfig::default(), &mut vec![]);
        assert!(ambient.is_ambient());
        assert!(
            ambient.config_dir.is_none(),
            "ambient keeps the user's gcloud"
        );

        // A configured key auto-scopes the gcloud config to a lazybox-owned
        // dir so the user's own is never touched.
        let sc = SandboxConfig {
            auth: lazybox_config::SandboxAuthConfig {
                service_account_key: Some(PathBuf::from("/keys/sa.json")),
                ..Default::default()
            },
            ..SandboxConfig::default()
        };
        let auth = resolve_auth(&sc, &mut vec![]);
        assert!(!auth.is_ambient());
        assert_eq!(
            auth.service_account_key.as_deref(),
            Some(std::path::Path::new("/keys/sa.json"))
        );
        assert_eq!(auth.config_dir, Some(default_gcloud_config_dir()));
        assert!(
            auth.config_dir
                .unwrap()
                .starts_with(lazybox_core::paths::state_root())
        );

        // Impersonation whose base is ambient/metadata must NOT auto-scope —
        // scoping to an empty dir would strand the base credential (finding #2).
        let sc = SandboxConfig {
            auth: lazybox_config::SandboxAuthConfig {
                impersonate_service_account: Some("deploy@p.iam.gserviceaccount.com".into()),
                ..Default::default()
            },
            ..SandboxConfig::default()
        };
        let auth = resolve_auth(&sc, &mut vec![]);
        assert!(!auth.is_ambient());
        assert!(
            auth.config_dir.is_none(),
            "impersonation-only must not scope gcloud away from its base"
        );
    }

    #[test]
    fn resolve_auth_takes_flags_over_config() {
        let sc = SandboxConfig {
            auth: lazybox_config::SandboxAuthConfig {
                service_account_key: Some(PathBuf::from("/cfg/sa.json")),
                ..Default::default()
            },
            ..SandboxConfig::default()
        };
        let mut args = vec![
            "--service-account-key".into(),
            "/flag/sa.json".into(),
            "--impersonate-service-account".into(),
            "deploy@p.iam.gserviceaccount.com".into(),
        ];
        let auth = resolve_auth(&sc, &mut args);
        assert_eq!(
            auth.service_account_key.as_deref(),
            Some(std::path::Path::new("/flag/sa.json"))
        );
        assert_eq!(
            auth.impersonate_service_account.as_deref(),
            Some("deploy@p.iam.gserviceaccount.com")
        );
    }

    #[test]
    fn state_file_is_isolated_per_worktree_key() {
        assert_ne!(
            state_file_for("/repos/foo/wt"),
            state_file_for("/repos/bar/wt")
        );
        assert!(state_file_for("/wt").ends_with("terraform.tfstate"));
    }

    #[test]
    fn an_explicit_empty_remote_socket_reaches_the_connect_guard() {
        // `remote_socket: ""` in YAML is the one input that must NOT be
        // silently replaced by the convention default — connect's
        // empty-socket bail exists exactly for it.
        let sc = SandboxConfig {
            remote_socket: Some(String::new()),
            ..SandboxConfig::default()
        };
        let p = resolve_provider(&sc, &mut vec![], "/wt").unwrap();
        assert!(p.remote_socket.is_empty());
    }

    #[test]
    fn resolve_deployment_defaults_and_surfaces_a_bad_overlay() {
        // No overlay → the embedded default recipe (what `connect` reads
        // workload ports from without needing a project).
        let sc = SandboxConfig::default();
        let d = resolve_deployment(&sc, &mut vec![]).unwrap();
        assert_eq!(d.config.name, "default");

        // A configured-but-missing overlay is an error, not a silent
        // fallback to the default recipe.
        let sc = SandboxConfig {
            deployment: Some(PathBuf::from("/definitely/not/here.yaml")),
            ..SandboxConfig::default()
        };
        assert!(resolve_deployment(&sc, &mut vec![]).is_err());
    }

    #[test]
    fn union_ports_appends_only_missing() {
        assert_eq!(
            union_ports(vec![22, 3000], &[3000, 8082]),
            vec![22, 3000, 8082]
        );
        assert_eq!(union_ports(vec![], &[3000]), vec![3000]);
    }

    #[test]
    fn install_lazybox_defaults_on_and_honors_an_explicit_opt_out() {
        assert!(
            install_lazybox_enabled(&SandboxConfig::default()),
            "the product path installs a daemon by default"
        );
        let opt_out = SandboxConfig {
            install_lazybox: Some(false),
            ..SandboxConfig::default()
        };
        assert!(!install_lazybox_enabled(&opt_out));
    }

    #[test]
    fn normalize_build_sha_blanks_the_non_git_sentinel() {
        assert_eq!(normalize_build_sha("abc123"), "abc123");
        // A non-git build (release tarball) or an empty value must NOT pin a
        // bogus commit — the box falls back to the default branch.
        assert_eq!(normalize_build_sha("unknown"), "");
        assert_eq!(normalize_build_sha(""), "");
    }

    #[test]
    fn resolve_provider_defaults_the_user_to_lazybox_when_installing() {
        // Installing the daemon → SSH in as the dedicated `lazybox` user so
        // the home-relative daemon socket resolves.
        let sc = SandboxConfig::default();
        let p = resolve_provider(&sc, &mut vec![], "/wt").unwrap();
        assert_eq!(p.user.as_deref(), Some(BOX_LAZYBOX_USER));

        // A bring-your-own-stack box keeps gcloud's default user…
        let opt_out = SandboxConfig {
            install_lazybox: Some(false),
            ..SandboxConfig::default()
        };
        assert_eq!(
            resolve_provider(&opt_out, &mut vec![], "/wt").unwrap().user,
            None
        );

        // …and an explicit user always wins.
        let explicit = SandboxConfig {
            user: Some("alice".into()),
            ..SandboxConfig::default()
        };
        assert_eq!(
            resolve_provider(&explicit, &mut vec![], "/wt")
                .unwrap()
                .user
                .as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn resolve_spec_carries_the_install_flag_and_client_sha() {
        let sc = SandboxConfig {
            project: Some("proj".into()),
            ..SandboxConfig::default()
        };
        let spec = resolve_spec(&sc, &mut vec![], "/home/me/wt").unwrap();
        assert!(spec.install_lazybox, "default installs the daemon");
        // The spec's SHA is exactly the normalized client build SHA.
        assert_eq!(spec.lazybox_git_sha, client_build_sha());

        let opt_out = SandboxConfig {
            project: Some("proj".into()),
            install_lazybox: Some(false),
            ..SandboxConfig::default()
        };
        let spec = resolve_spec(&opt_out, &mut vec![], "/home/me/wt").unwrap();
        assert!(!spec.install_lazybox);
    }

    #[test]
    fn resolve_spec_requires_a_project() {
        let sc = SandboxConfig::default();
        let mut args = vec![];
        assert!(resolve_spec(&sc, &mut args, "/wt").is_err());
    }

    #[test]
    fn resolve_spec_defaults_placement_and_uses_the_embedded_default_recipe() {
        let sc = SandboxConfig {
            project: Some("proj".into()),
            ..SandboxConfig::default()
        };
        let mut args = vec![];
        let spec = resolve_spec(&sc, &mut args, "/home/me/wt").unwrap();
        assert_eq!(spec.project, "proj");
        assert_eq!(spec.region, "us-central1");
        assert_eq!(spec.zone, "us-central1-a");
        assert!(spec.name.starts_with("lazybox-sbx-wt-"), "{}", spec.name);
        // No overlay → the generic default recipe.
        assert_eq!(spec.deployment.config.name, "default");
    }

    #[test]
    fn dirty_build_warning_fires_only_for_a_dirty_client() {
        // A clean build can build a matching daemon → no warning.
        assert!(dirty_build_warning("0.1.0+abc123def456").is_none());
        // A dirty build resolves to the clean commit, so parity can't hold —
        // the warning must fire and name the remedy.
        let w = dirty_build_warning("0.1.0+abc123def456-dirty").expect("dirty client warns");
        assert!(w.contains("parity"), "{w}");
    }

    /// A scripted [`SandboxProvider`] whose `status` reports the box as
    /// reachable only from the Nth call onward — so the wake→sshd wait is
    /// exercised without a live box.
    struct FakeBox {
        power: PowerState,
        reachable_after: usize,
        calls: std::cell::Cell<usize>,
    }

    impl SandboxProvider for FakeBox {
        fn id(&self) -> &str {
            "fake"
        }
        async fn ensure(
            &self,
            _s: &SandboxSpec,
        ) -> Result<BoxHandle, lazybox_sandbox::SandboxError> {
            unreachable!("wait test never ensures")
        }
        async fn start(&self, _h: &BoxHandle) -> Result<(), lazybox_sandbox::SandboxError> {
            Ok(())
        }
        async fn stop(&self, _h: &BoxHandle) -> Result<(), lazybox_sandbox::SandboxError> {
            Ok(())
        }
        async fn status(
            &self,
            _h: &BoxHandle,
        ) -> Result<lazybox_sandbox::BoxStatus, lazybox_sandbox::SandboxError> {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            Ok(lazybox_sandbox::BoxStatus {
                power: self.power,
                reachable: n >= self.reachable_after,
            })
        }
        async fn connect(
            &self,
            _h: &BoxHandle,
            _p: &[u16],
        ) -> Result<lazybox_sandbox::Tunnel, lazybox_sandbox::SandboxError> {
            unreachable!("wait test never connects")
        }
        async fn destroy(&self, _h: &BoxHandle) -> Result<(), lazybox_sandbox::SandboxError> {
            Ok(())
        }
    }

    fn fake_handle() -> BoxHandle {
        BoxHandle {
            provider: "fake".into(),
            id: "lazybox-sbx-test".into(),
            region: "r".into(),
            zone: "z".into(),
            project: "p".into(),
            power_state: PowerState::Running,
            last_active: None,
        }
    }

    // Tiny real durations (not paused time, which needs tokio's test-util
    // feature) keep these well under the CI per-test deadline.
    #[tokio::test]
    async fn wait_until_reachable_returns_once_sshd_answers() {
        // Reachable on the 2nd probe: the wait must poll past the first
        // not-yet-reachable result instead of running the rebuild ssh blind.
        let fake = FakeBox {
            power: PowerState::Running,
            reachable_after: 2,
            calls: std::cell::Cell::new(0),
        };
        wait_until_reachable(
            &fake,
            &fake_handle(),
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await
        .expect("becomes reachable within the window");
        assert_eq!(fake.calls.get(), 2, "polled until reachable");
    }

    #[tokio::test]
    async fn wait_until_reachable_times_out_when_sshd_never_answers() {
        // A box that never accepts SSH must bail at the deadline, not loop
        // forever — the caller then reports a clear failure.
        let fake = FakeBox {
            power: PowerState::Running,
            reachable_after: usize::MAX,
            calls: std::cell::Cell::new(0),
        };
        let out = wait_until_reachable(
            &fake,
            &fake_handle(),
            Duration::from_millis(30),
            Duration::from_millis(2),
        )
        .await;
        assert!(out.is_err(), "an unreachable box must time out");
    }
}
