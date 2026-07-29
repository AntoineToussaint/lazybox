//! `SessionBackend` impl backed by `tmux`.
//!
//! Why tmux? Sessions outlive the lazybox server. If lazybox crashes or is
//! restarted, the user's Claude conversation, build output, and shell
//! history all survive — `backend.list()` rediscovers them and the
//! server reattaches a fresh I/O conduit. The user can also
//! `tmux -L lazybox attach -t <key>` from any other terminal to see and
//! drive the same session — useful for debugging or for picking up
//! work from a different machine over SSH.
//!
//! ## Wire model
//!
//! Each session is a tmux session in its own right under the
//! private socket `tmux -L lazybox`. The lazybox server keeps **one
//! attached portable-pty client per session** as the I/O conduit:
//! bytes the client renders → broadcast to subscribers; bytes from
//! `write()` → fed to the client's stdin, which tmux relays to the
//! agent process inside.
//!
//! This is intentionally a "headless tmux client" — a custom config
//! file disables tmux's prefix key, status bar, and key bindings so
//! the bytes flowing through are the agent's own output, not framed
//! by tmux UI. The agent inside doesn't know it's in tmux; lazybox's
//! libghostty-vt parser doesn't know either; the only role tmux plays
//! is to keep the inner PTY alive when no lazybox client is attached.
//!
//! ## Restart recovery
//!
//! `list()` shells out to `tmux list-sessions` so the server can
//! enumerate sessions that survived a restart. To rebind one, the
//! caller invokes `subscribe(key)` which spawns a fresh tmux-attach
//! client — output streams as if it had just been spawned, and any
//! pending write goes to the existing inner process.

use crate::backend::{BackendError, OutputChunk, SessionBackend, Subscription};
use crate::pty::DaemonPty;
use portable_pty::PtySize;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

/// Default socket name for the lazybox-owned tmux server. Isolating to
/// a private socket means we never touch the user's interactive tmux
/// sessions running on the default socket.
///
/// The runtime socket name comes from
/// [`lazybox_core::paths::tmux_socket_name`] which derives a unique
/// name per profile (`LAZYBOX_HOME=~/.lazybox-dev` → `lazybox-dev`). The
/// constant below is kept for backward compatibility with callers
/// that imported it directly.
pub const TMUX_SOCKET: &str = "lazybox";

/// The `terminal-overrides` entry that keeps tmux's attach client off
/// the alternate screen. `DaemonPty::spawn` forces `TERM=xterm-256color`
/// for the attach client, so the `xterm*` pattern always matches it.
/// With smcup/rmcup disabled, tmux writes the pane in place on the
/// outer terminal and scrolled-out lines flow into the client's own
/// (libghostty) scrollback instead of vanishing into tmux's alternate
/// screen. Panes keep their own alternate screens inside tmux's model —
/// an inner vim/less still renders normally; only tmux's use of the
/// OUTER terminal changes.
const SCROLLBACK_TERMINAL_OVERRIDES: &str = ",xterm*:smcup@:rmcup@";

/// Per-pane scrollback depth tmux retains. Set as `history-limit` in the
/// conf and used as the `-S` start line when capturing history to seed a
/// reattaching client (`capture_history`), so the two never drift.
const HISTORY_LIMIT: u32 = 10_000;

/// Minimum tmux version the backend's conf requires, enforced by
/// [`TmuxBackend::detect`] (older tmux → raw-PTY fallback, same as no
/// tmux at all). Pinned by the newest option the conf sets:
/// `allow-passthrough` (tmux 3.3).
///
/// Old tmux must be REJECTED, not tolerated: an option unknown to the
/// running tmux is a conf parse error, and tmux swaps the first
/// attaching client into a "config error" view instead of the pane —
/// which never repaints until a key is pressed, so lazybox's headless
/// attach client streams no pane content at all. Every session on that
/// host is silently dead (seen on Ubuntu 22.04 LTS, tmux 3.2a).
pub const MIN_TMUX_VERSION: (u32, u32) = (3, 3);

/// Clipboard passthrough options, independent of scrollback handling.
/// `set-clipboard on` forwards an inner program's OSC 52 to the attach
/// client; `allow-passthrough on` (off by default since tmux 3.3a) lets
/// a DCS-wrapped escape through. Without these tmux eats the clipboard
/// request before lazybox can relay it to the host terminal.
const CLIPBOARD_PASSTHROUGH_OPTS: &str = "set -g set-clipboard on\nset -g allow-passthrough on\n";

/// `terminal-features` entry advertising OSC 8 hyperlink support for the
/// attach client. `terminal-features` is a server option (`-s`) and a
/// list (`-a` append); the `xterm*` pattern matches the forced
/// `TERM=xterm-256color`. Without it tmux strips hyperlinks because that
/// terminfo lacks the `Hls` capability — see `transparent_conf`.
const HYPERLINK_TERMINAL_FEATURES_VALUE: &str = "xterm*:hyperlinks";
const HYPERLINK_TERMINAL_FEATURES: &str = "set -as terminal-features 'xterm*:hyperlinks'\n";

/// tmux client config: prefix off (so Ctrl-B reaches the agent), no
/// key bindings (so nothing intercepts), no status bar (so output
/// isn't framed). Built as a string and dropped to a temp file at
/// `TmuxBackend::new` time so we don't depend on the user's
/// `~/.tmux.conf`.
///
/// tmux mouse handling stays off and both sides of the conduit reject
/// alternate screens. The pane therefore retains output in tmux history,
/// while the lazybox client accumulates the relayed output in its local
/// 10k-line scrollback. Inner mouse modes still pass through for clicks;
/// wheel events never leave the client.
///
/// Every option here may assume [`MIN_TMUX_VERSION`] — `detect()`
/// refuses older tmux, because a single unknown option in this conf
/// breaks every attach (see the constant's doc). Raising an option's
/// floor means raising `MIN_TMUX_VERSION` with it.
fn transparent_conf() -> String {
    let mut conf = String::from(
        "set -g prefix None\n\
         set -g status off\n\
         set -g mouse off\n",
    );
    conf.push_str(&format!("set -g history-limit {HISTORY_LIMIT}\n"));
    // Lazybox scrollback exists ONLY for pane content that scrolls into
    // tmux history — the restart seed, the live deep-scrollback fetch,
    // and the attach client's own accumulation all read from the primary
    // screen's scroll-off. A program that switches its pane to the
    // alternate screen (Claude Code ≥2.1 does) accumulates ZERO history:
    // scrollback silently dies for that session, live and after restart
    // alike. Deny the alt screen at the pane level so agent output keeps
    // flowing where history is retained — the pane-side counterpart of
    // the attach-side `smcup@/rmcup@` stripping below.
    conf.push_str("set -g alternate-screen off\n");
    conf.push_str(
        "set -g default-terminal \"xterm-256color\"\n\
         set -g escape-time 0\n\
         set -g window-size latest\n\
         set -g mode-style \"fg=default,bg=default\"\n\
         set -g message-style \"fg=default,bg=default\"\n\
         set -g focus-events on\n",
    );
    // Clipboard passthrough. `set-clipboard on` lets an inner program's
    // OSC 52 reach the attach client (which relays it to the host), and
    // `allow-passthrough on` lets a DCS-wrapped escape (some agents wrap
    // OSC 52 / banners in tmux's passthrough envelope) through untouched.
    // Without these tmux swallows the clipboard request and the lazybox
    // forwarder never sees it.
    conf.push_str(CLIPBOARD_PASSTHROUGH_OPTS);
    // Tell tmux the attach client understands OSC 8 hyperlinks so it
    // RE-EMITS them instead of stripping. We force the client's
    // `TERM=xterm-256color`, whose terminfo has no `Hls` capability,
    // so without this tmux drops every hyperlink an inner program
    // (claude, gh, `ls --hyperlink`) emits — and right-click-to-open
    // never sees a URI because the payload is gone before it reaches
    // libghostty's VT parser. The `xterm*` pattern matches the forced
    // client TERM, same as `terminal-overrides` above.
    conf.push_str(HYPERLINK_TERMINAL_FEATURES);
    conf.push_str("set -g terminal-overrides '");
    conf.push_str(SCROLLBACK_TERMINAL_OVERRIDES);
    conf.push_str("'\n");
    conf.push_str("unbind-key -a\n");
    conf
}

/// Turn `tmux capture-pane -p` output into replay-ring bytes.
///
/// capture-pane separates pane lines with a bare `\n`. The reattaching
/// client's VT parser runs with LNM off, where `\n` is a plain line feed
/// that moves the cursor down WITHOUT returning to column 0 — feeding the
/// capture verbatim would staircase every line. So each `\n` becomes
/// `\r\n`.
///
/// capture-pane also pads its output to the full pane grid: unused and
/// cleared rows at the bottom of the screen come back as blank lines that
/// were never real scrollback. Seeding them verbatim injects spurious
/// empty rows into the reconstructed history — the "random extra empty
/// lines after restart / move-session" (#442). Every trailing blank row is
/// dropped (subsuming the single separating newline capture appends), so
/// the seed ends exactly at the last row with real content — where tmux's
/// live attach repaint resumes, stitching seeded history to the live
/// screen without a blank row between them. Only the trailing run is
/// trimmed: interior blank lines are genuine output and survive untouched.
///
/// A genuine blank line at the very BOTTOM of the output is
/// indistinguishable from grid padding and is trimmed with it — but the
/// live attach repaint redraws the true bottom-of-screen over the seed, so
/// nothing visible is lost. Reconstructing that boundary faithfully is the
/// lossy screen-grid problem #393 tracks separately.
fn normalize_capture(stdout: &[u8]) -> Vec<u8> {
    // Peel the trailing run of blank rows via the shared scrollback-seed
    // helper, so this path and the raw-PTY reseed detect blank rows
    // identically and never drift. Unlike the raw seed, the trailing
    // terminator is dropped entirely: the seed ends exactly at the last
    // content row, where tmux's live attach repaint resumes.
    let trimmed = &stdout[..crate::pty::content_end(stdout)];
    let newlines = trimmed.iter().filter(|&&b| b == b'\n').count();
    let mut seed = Vec::with_capacity(trimmed.len() + newlines);
    for &b in trimmed {
        if b == b'\n' {
            seed.push(b'\r');
        }
        seed.push(b);
    }
    seed
}

/// `set-option` invocations that bring an ALREADY-RUNNING tmux server
/// (started by an older lazybox) in line with lazybox's scrollback model.
/// The `-f` conf only applies at tmux server
/// start, so sessions that survived a lazybox upgrade would otherwise
/// keep the old mouse / alt-screen behavior forever. Applied once per
/// backend process, before the first attach, so re-attached clients
/// pick the overrides up (terminal-overrides is consulted at client
/// attach time).
fn server_option_cmds() -> Vec<Vec<&'static str>> {
    // Clipboard passthrough is independent of scrollback handling, so
    // an already-running server picks it up either way.
    let clipboard = [
        vec!["set-option", "-g", "set-clipboard", "on"],
        vec!["set-option", "-g", "allow-passthrough", "on"],
    ];
    // Agents (e.g. Claude Code) nag when they detect tmux with
    // focus-events off. We own the config, so enable it for both fresh
    // and recovered servers.
    let focus_events = vec!["set-option", "-g", "focus-events", "on"];
    // Pane-level alt-screen denial (see `transparent_conf`): a
    // pre-existing server from an older lazybox still allows it, and
    // every alt-screen pane retains zero history — pushing this heals
    // sessions spawned after the push (a pane already inside the alt
    // screen stays there until its program leaves it).
    let no_alt_screen = vec!["set-option", "-g", "alternate-screen", "off"];
    // `terminal-features` is independent of scrollback handling — an
    // already-running server must learn the client speaks OSC 8 either
    // way, else surviving sessions keep stripping hyperlinks.
    let hyperlinks = vec![
        "set-option",
        "-as",
        "terminal-features",
        HYPERLINK_TERMINAL_FEATURES_VALUE,
    ];
    let mut cmds = vec![
        vec!["set-option", "-g", "mouse", "off"],
        vec![
            "set-option",
            "-g",
            "terminal-overrides",
            SCROLLBACK_TERMINAL_OVERRIDES,
        ],
        hyperlinks,
    ];
    cmds.extend(clipboard);
    cmds.push(focus_events);
    cmds.push(no_alt_screen);
    cmds
}

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Wall-clock cap on every tmux subprocess invocation. tmux commands
/// are local and complete in milliseconds; a tmux server wedged on a
/// dead socket (or a hung first-start) must surface as an error, not
/// freeze whichever daemon task awaited it. `kill_on_drop` on the
/// commands ensures a timed-out tmux child is reaped.
const TMUX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-session state. The DaemonPty is the tmux-attach client that
/// streams I/O between lazybox and the underlying tmux session.
struct Slot {
    /// The portable-pty wrapping `tmux attach -t <key>`.
    /// Killed when `kill()` is called; if the session itself ends,
    /// tmux exits the client which trips DaemonPty's EOF.
    client: Arc<DaemonPty>,
}

pub struct TmuxBackend {
    /// `tmux -L <socket>` socket name. Per-process so multiple lazybox
    /// processes don't share session state by accident.
    socket: String,
    /// Path to the transparent-tmux config dropped on first call.
    /// Persists for the process lifetime; cleaned up on Drop.
    config_path: PathBuf,
    /// Rendered conf contents. Kept so the self-heal rewrite in `tmux()`
    /// reproduces the same configuration.
    conf: String,
    /// Whether `server_option_cmds` has been applied to the (possibly
    /// pre-existing) tmux server this process talks to. Once per
    /// process, before the first attach. A `OnceCell` (not an
    /// `AtomicBool` swap) so concurrent first spawns AWAIT the winner
    /// finishing the option setup — with the swap, the loser raced
    /// ahead and attached before the options had landed.
    options_applied: tokio::sync::OnceCell<()>,
    sessions: Mutex<HashMap<String, Slot>>,
    next_key: AtomicU64,
}

/// Pull `(major, minor)` out of a `tmux -V` banner. Handles the release
/// shape (`tmux 3.2a`), the development shape (`tmux next-3.4`), and
/// distro decorations, by parsing the first `digits.digits` run and
/// ignoring any patch-letter suffix. `None` for banners with no such
/// run (e.g. OpenBSD's `tmux openbsd-7.6`, which tracks upstream head
/// and is always modern).
fn parse_tmux_version(banner: &str) -> Option<(u32, u32)> {
    let start = banner.find(|c: char| c.is_ascii_digit())?;
    let rest = &banner[start..];
    let major_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let major = rest[..major_end].parse().ok()?;
    let rest = rest[major_end..].strip_prefix('.')?;
    let minor_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let minor = rest[..minor_end].parse().ok()?;
    Some((major, minor))
}

/// Probe PATH for a tmux that satisfies [`MIN_TMUX_VERSION`]. Returns
/// the `tmux -V` banner when usable, `None` (with the reason logged)
/// when tmux is missing or too old. A banner with no parseable version
/// is treated as modern — those are development or vendor builds that
/// track upstream head.
pub fn modern_tmux_version() -> Option<String> {
    let out = Command::new("tmux").arg("-V").output().ok()?;
    if !out.status.success() {
        tracing::debug!("tmux -V failed; tmux backend unavailable");
        return None;
    }
    let banner = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if let Some(version) = parse_tmux_version(&banner)
        && version < MIN_TMUX_VERSION
    {
        let (maj, min) = MIN_TMUX_VERSION;
        tracing::warn!(
            "{banner} is older than the required tmux {maj}.{min} — \
             sessions will NOT survive lazybox restarts (raw-PTY \
             fallback). Upgrade tmux to re-enable persistent sessions."
        );
        return None;
    }
    Some(banner)
}

impl TmuxBackend {
    /// Probe for tmux on PATH and write the transparent config. Returns
    /// `None` when tmux isn't usable on this machine — missing entirely
    /// or older than [`MIN_TMUX_VERSION`] — and callers fall back to
    /// `RawPtyBackend`.
    pub fn detect() -> Option<Self> {
        let version = modern_tmux_version()?;
        tracing::info!("tmux backend available: {version}");
        // Profile-aware socket name. Default profile resolves to
        // "lazybox" — backward compatible with running sessions; a
        // dev profile (`LAZYBOX_HOME=~/.lazybox-dev`) gets "lazybox-dev"
        // so two lazybox daemons don't share session state.
        let socket = lazybox_core::paths::tmux_socket_name();
        Self::with_socket(&socket).ok()
    }

    /// Build a backend pinned to a specific tmux socket name. Useful for
    /// tests so concurrent runs don't share state.
    pub fn with_socket(socket: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join("lazybox-tmux");
        std::fs::create_dir_all(&dir)?;
        let config_path = dir.join(format!("{socket}.conf"));
        let conf = transparent_conf();
        std::fs::write(&config_path, &conf)?;
        Ok(Self {
            socket: socket.into(),
            config_path,
            conf,
            options_applied: tokio::sync::OnceCell::new(),
            sessions: Mutex::new(HashMap::new()),
            next_key: AtomicU64::new(1),
        })
    }

    fn alloc_key(&self, hint: &str) -> String {
        let n = self.next_key.fetch_add(1, Ordering::Relaxed);
        // Format: `lazybox-{hint}-{pid}-{n}`. The hint is a readable
        // seed (`widget-126-claude`) so `tmux ls` shows what
        // each session is for; PID + counter guarantee uniqueness
        // across lazybox launches (PIDs aren't reused while in-use)
        // and within a single process. Recovery is name-agnostic
        // so old sessions still get reattached by their existing
        // names on restart.
        //
        // Sanitize the hint: tmux session names can't contain
        // '.', ':', or whitespace. Replace any with '-'.
        let safe: String = hint
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("lazybox-{safe}-{}-{n}", std::process::id())
    }

    /// Run `tmux -L <socket> -f <config> ...args`, returning its output
    /// even when tmux exits non-zero.
    ///
    /// **Async**: `tokio::process::Command` rather than the sync std
    /// version. The backend's trait methods are wrapped in async
    /// futures, and a sync `output()` here would block the entire
    /// tokio runtime for the duration of every tmux invocation —
    /// which can be 100ms+ during server startup. Every other task
    /// (TUI render, IPC pumps, polling) would freeze in lockstep.
    async fn tmux_output(&self, args: &[&str]) -> Result<std::process::Output, BackendError> {
        // Self-heal the conf file: another lazybox instance running on
        // the same machine could have hit `Drop` and removed it (we
        // share `std::env::temp_dir()/lazybox-tmux/lazybox.conf` across
        // instances). Re-writing every call is cheap (a few hundred
        // bytes) and beats a confusing "No such file" error on the
        // user's first spawn.
        if !self.config_path.exists() {
            if let Some(parent) = self.config_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.config_path, &self.conf);
        }
        let fut = tokio::process::Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .arg("-f")
            .arg(&self.config_path)
            .args(args)
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(TMUX_TIMEOUT, fut)
            .await
            .map_err(|_| {
                BackendError::Other(format!(
                    "tmux {} timed out after {}s",
                    args.join(" "),
                    TMUX_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| BackendError::Other(format!("tmux invoke: {e}")))?;
        Ok(out)
    }

    /// Run tmux and require a successful exit status.
    async fn tmux(&self, args: &[&str]) -> Result<std::process::Output, BackendError> {
        let out = self.tmux_output(args).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(BackendError::Other(format!(
                "tmux {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(out)
    }

    /// Apply `server_option_cmds` to the tmux server once per backend
    /// process. The `-f` conf only takes effect when a tmux command
    /// STARTS the server, so a server left running by an older lazybox
    /// keeps its options across our restarts — this pushes the current
    /// settings onto it explicitly so both freshly
    /// spawned and recovered sessions behave the same. Best-effort:
    /// a failure degrades scrolling, it must not block the spawn.
    async fn ensure_server_options(&self) {
        // `get_or_init` makes concurrent callers WAIT for the first
        // one to finish setting the options, instead of attaching
        // against a half-configured server.
        self.options_applied
            .get_or_init(|| async {
                for cmd in server_option_cmds() {
                    if let Err(e) = self.tmux(&cmd).await {
                        tracing::warn!("tmux server option setup failed: {e}");
                    }
                }
            })
            .await;
    }

    /// Build the portable-pty argv for an attaching tmux client. We
    /// pass `-f` so the client uses the transparent config too — this
    /// is what disables status/prefix/bindings on the rendering side.
    fn attach_argv(&self, key: &str) -> Vec<String> {
        vec![
            "tmux".into(),
            "-L".into(),
            self.socket.clone(),
            "-f".into(),
            self.config_path.to_string_lossy().into_owned(),
            "attach".into(),
            "-t".into(),
            key.into(),
        ]
    }

    /// Spawn the tmux-attach DaemonPty for `key`. The attach client
    /// is the I/O conduit; its lifetime is unrelated to the tmux
    /// session's — callers must use `is_alive` to distinguish conduit
    /// failure from session death.
    ///
    /// `seed` hands the DaemonPty reconstructed scrollback (see
    /// `capture_history`), held in its durable seed slot and replayed
    /// ahead of live bytes (#420). A plain `tmux attach` only
    /// repaints the visible pane, so without the seed a client that
    /// reattaches to a session which survived a daemon restart has one
    /// screenful and nothing to scroll back through.
    fn open_client(&self, key: &str, size: PtySize, seed: &[u8]) -> Result<Slot, BackendError> {
        let argv = self.attach_argv(key);
        let pty = DaemonPty::spawn_relay(&argv, size, None, Vec::new(), seed)
            .map_err(|e| BackendError::Spawn(format!("tmux attach: {e}")))?;
        Ok(Slot {
            client: Arc::new(pty),
        })
    }

    /// The pane's current cell grid, `(cols, rows)`. A session that
    /// survived a daemon restart keeps the size its last client set it to
    /// (`window-size latest`), so reattaching at exactly this size means
    /// tmux reports no resize to the pane and the inner program never runs
    /// its reattach redraw — the DEFAULT-size→viewport reflow that left a
    /// blank gap under the recovered prompt box (#533). `None` when tmux
    /// cannot report the size, and the caller falls back to the default
    /// grid.
    async fn pane_size(&self, key: &str) -> Option<(u16, u16)> {
        let out = self
            .tmux(&[
                "display-message",
                "-p",
                "-t",
                key,
                "#{pane_width}\t#{pane_height}",
            ])
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.trim().split('\t');
        let cols: u16 = parts.next()?.parse().ok()?;
        let rows: u16 = parts.next()?.parse().ok()?;
        // A zero dimension would attach a 0-cell PTY. tmux never reports
        // one for a live pane, but treat it as "unknown" so the caller
        // falls back to the default grid rather than a degenerate size.
        (cols > 0 && rows > 0).then_some((cols, rows))
    }

    /// Whether the pane is alternate and how many history lines tmux
    /// currently retains. `None` when tmux cannot report the state.
    async fn pane_history_state(&self, key: &str) -> Option<(bool, u64)> {
        let out = self
            .tmux(&[
                "display-message",
                "-p",
                "-t",
                key,
                "#{alternate_on}\t#{history_size}",
            ])
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.trim().split('\t');
        Some((parts.next()? == "1", parts.next()?.parse().ok()?))
    }

    /// Reconstruct a reattaching client's scrollback from tmux's own
    /// history. The daemon's replay ring lives in memory and dies with
    /// the daemon; tmux, however, keeps `history-limit` lines per pane
    /// across restarts. `capture-pane -e -J -S -<limit>` dumps that history
    /// (styled, via `-e`) and joins tmux's soft-wrapped screen rows back into
    /// logical lines (`-J`) before we hand it to the DaemonPty as its durable
    /// seed. The seed is replayed ahead of live attach bytes on every snapshot
    /// (#420), so the client rebuilds full scrollback without hardening wraps.
    ///
    /// Best-effort: any failure returns an empty seed and the client
    /// simply starts from the live repaint.
    async fn capture_history(&self, key: &str) -> Vec<u8> {
        let start = format!("-{HISTORY_LIMIT}");
        let out = match self
            .tmux(&["capture-pane", "-p", "-e", "-J", "-S", &start, "-t", key])
            .await
        {
            Ok(out) => out.stdout,
            Err(e) => {
                tracing::warn!(key, "tmux capture-pane for scrollback seed failed: {e}");
                return Vec::new();
            }
        };
        let seed = normalize_capture(&out);
        if let Some(raw) = crate::pty::debug_byte_fingerprint(&out) {
            let normalized = crate::pty::byte_fingerprint(&seed);
            tracing::debug!(
                key,
                raw_len = raw.len,
                raw_newlines = raw.newlines,
                raw_hash = raw.hash,
                seed_len = normalized.len,
                seed_newlines = normalized.newlines,
                seed_hash = normalized.hash,
                "tmux capture normalized at reattach boundary"
            );
        }
        seed
    }
}

impl Drop for TmuxBackend {
    fn drop(&mut self) {
        // We deliberately do NOT kill the tmux server or remove the
        // conf file here. Two reasons:
        //
        // 1. Sessions persist across lazybox restarts — that's the
        //    whole point of the tmux backend (claude/codex sessions
        //    survive a `q q`/relaunch). Killing the server defeats it.
        // 2. Multiple lazybox instances on the same host share the
        //    socket + conf. A Drop-on-exit by one instance would yank
        //    the conf out from under a still-running sibling, which
        //    is the bug that surfaced as "No such file or directory"
        //    on the user's terminal spawns.
        //
        // Stale tmux servers + conf files in $TMPDIR are cheap; the
        // OS reaps them at reboot.
    }
}

/// Build `tmux new-session -d -s <key> -x <cols> -y <rows> [-c <cwd>]
/// [-e K=V ...] -- <argv...>`. Detached session — the caller attaches
/// its own client afterwards.
///
/// Besides the caller's env, `-e COLORTERM=truecolor` is always seeded
/// into the session environment. `TERM` inside the session comes from
/// the conf's `default-terminal "xterm-256color"`, but tmux does not
/// propagate the attach client's `COLORTERM`, so without this the
/// inner program (Codex among them) fails its truecolor probe and
/// renders degraded/monochrome (#421). Seeded after the caller's env,
/// mirroring how `DaemonPty::spawn` forces the pair.
fn new_session_args(
    key: &str,
    cwd: Option<&Path>,
    env: &[(String, String)],
    argv: &[String],
) -> Vec<String> {
    let mut cmd_args: Vec<String> = vec![
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        key.to_string(),
        "-x".into(),
        DEFAULT_COLS.to_string(),
        "-y".into(),
        DEFAULT_ROWS.to_string(),
    ];
    if let Some(dir) = cwd {
        cmd_args.push("-c".into());
        cmd_args.push(dir.to_string_lossy().into_owned());
    }
    for (k, v) in env {
        cmd_args.push("-e".into());
        cmd_args.push(format!("{k}={v}"));
    }
    cmd_args.push("-e".into());
    cmd_args.push("COLORTERM=truecolor".into());
    // `--` separator so argv elements starting with `-` are
    // treated as command tokens, not tmux flags.
    cmd_args.push("--".into());
    cmd_args.extend(argv.iter().cloned());
    cmd_args
}

impl SessionBackend for TmuxBackend {
    fn id(&self) -> &'static str {
        "tmux"
    }

    fn spawn<'a>(
        &'a self,
        argv: &'a [String],
        cwd: Option<&'a Path>,
        env: &'a [(String, String)],
        hint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            if argv.is_empty() {
                return Err(BackendError::Spawn("empty argv".into()));
            }
            let key = self.alloc_key(hint);

            // A server that survived an older lazybox still has its old
            // global options. Push the history-preserving settings before
            // the new program can request an alternate screen. With no
            // running server, `new-session` below starts one from `conf`.
            if self.tmux(&["list-sessions"]).await.is_ok() {
                self.ensure_server_options().await;
            }

            let cmd_args = new_session_args(&key, cwd, env, argv);
            let arg_refs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
            self.tmux(&arg_refs).await?;

            // Server is definitely up now — make sure its options
            // match this backend's scrollback mode before the attach
            // client connects (terminal-overrides binds at attach).
            self.ensure_server_options().await;

            // Now open the attaching client. If this fails the tmux
            // session is orphaned; tear it down so we don't leak.
            let size = PtySize {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                pixel_width: 0,
                pixel_height: 0,
            };
            // Fresh session: tmux has no history yet, so nothing to seed.
            let slot = match self.open_client(&key, size, &[]) {
                Ok(s) => s,
                Err(e) => {
                    let _ = self.tmux(&["kill-session", "-t", &key]).await;
                    return Err(e);
                }
            };
            self.sessions.lock().await.insert(key.clone(), slot);
            Ok(key)
        })
    }

    fn write<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.client.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            pty.write(bytes)
                .await
                .map_err(|e| BackendError::Other(e.to_string()))
        })
    }

    fn resize<'a>(
        &'a self,
        key: &'a str,
        cols: u16,
        rows: u16,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // Resizing the attached client's PTY makes tmux resize the
            // pane automatically: with `window-size latest` (pinned in
            // `transparent_conf`, not left to tmux's implicit default)
            // our refreshed client becomes the size authority, so a
            // second client attached elsewhere at a different size no
            // longer forces ours. (The previous `refresh-client -t
            // <session> -C` nudge was a dead no-op — `-t` takes a target
            // CLIENT, not a session, and `-C` is control-mode only, so it
            // always errored and was swallowed.)
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.client.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            pty.resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .await
            .map_err(|e| BackendError::Other(e.to_string()))?;
            Ok(())
        })
    }

    fn kill<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // Kill the tmux session FIRST, before dropping our slot.
            // Removing the slot up front made a failed kill
            // unretryable: a timed-out/unreachable tmux left the
            // session alive but the map empty, so a second Close
            // no-op'd forever.
            //
            // Idempotent: a session that's already gone makes tmux
            // exit non-zero ("can't find session" / "no server
            // running") — that's success for our purposes. Only a
            // TRANSPORT failure (invoke error, timeout) aborts and
            // keeps the slot so the caller can retry.
            match self.tmux(&["kill-session", "-t", key]).await {
                Ok(_) => {}
                Err(e @ BackendError::Other(_)) => {
                    let msg = e.to_string();
                    if msg.contains("timed out") || msg.contains("tmux invoke") {
                        tracing::warn!(
                            key,
                            "tmux kill-session failed — keeping slot for retry: {msg}"
                        );
                        return Err(e);
                    }
                    // Non-zero exit: session already gone. Fall through
                    // to release the conduit.
                }
                Err(e) => return Err(e),
            }
            // Session is down (or never existed) — drop the attach
            // client conduit and its slot.
            let slot = self.sessions.lock().await.remove(key);
            if let Some(slot) = slot {
                slot.client.kill();
            }
            Ok(())
        })
    }

    fn release<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // Pump-teardown path: the attach client already EOF'd (the
            // session ended, or the client detached). Drop only OUR
            // conduit slot — never `kill-session`: on a detach the
            // underlying tmux session may still be alive, and it must
            // stay discoverable for restart recovery.
            self.sessions.lock().await.remove(key);
        })
    }

    fn list<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // tmux itself is the lifecycle source of truth. The
            // in-memory map contains attach-client conduits, which may
            // remain after their session has died and must not make a
            // dead process recoverable.
            let mut keys = Vec::new();
            // `tmux list-sessions -F '#{session_name}'` — prints one
            // name per line. Empty stdout / no-server errors mean
            // "no sessions"; we treat them as Ok([]). Async to avoid
            // blocking the runtime on a slow tmux server start.
            let fut = tokio::process::Command::new("tmux")
                .arg("-L")
                .arg(&self.socket)
                .arg("-f")
                .arg(&self.config_path)
                .args(["list-sessions", "-F", "#{session_name}"])
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(TMUX_TIMEOUT, fut)
                .await
                .map_err(|_| {
                    BackendError::Other(format!(
                        "tmux list-sessions timed out after {}s",
                        TMUX_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|e| BackendError::Other(format!("tmux list: {e}")))?;
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let name = line.trim().to_string();
                    if !name.is_empty() {
                        keys.push(name);
                    }
                }
            }
            keys.sort();
            keys.dedup();
            Ok(keys)
        })
    }

    fn snapshot<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<
        Box<dyn Future<Output = Result<crate::backend::ReplaySnapshot, BackendError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Only return a snapshot if a client is already bound — we
            // don't want a snapshot probe to lazily spin up a tmux
            // client for a session that no one is subscribed to.
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|slot| slot.client.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            Ok(pty.snapshot_only().await)
        })
    }

    fn subscribe<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Subscription, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // If we already have a client open, share its broadcast.
            // If this is a session that survived restart and we
            // haven't bound a client yet, open one lazily. Recovered
            // sessions ride a tmux server an OLDER lazybox started —
            // push the current options before attaching so they
            // pick up the scrollback behavior too.
            self.ensure_server_options().await;
            // Reuse the cached client only if it's still ALIVE. A
            // `freeze` (detach-client) or any attach-client EOF makes
            // the DaemonPty finish while the inner tmux session keeps
            // running, but the dead Slot lingers in the map. Cloning
            // it would hand back a closed broadcast — stale replay,
            // no live output, no recovery. Detect that and open a
            // fresh attach client instead, replacing the dead Slot.
            let cached = {
                let mut map = self.sessions.lock().await;
                if map.get(key).is_some_and(|slot| !slot.client.is_finished()) {
                    map.get(key).map(|slot| slot.client.clone())
                } else {
                    // Drop a finished conduit BEFORE allocating its
                    // replacement. One relay owns three PTY descriptors and
                    // a socketpair; retaining those while `openpty()` tries
                    // to reserve the next set makes recovery impossible when
                    // the process is already close to RLIMIT_NOFILE.
                    map.remove(key);
                    None
                }
            };
            let pty = if let Some(pty) = cached {
                pty
            } else {
                // No live client — this is the reattach path (recovery
                // after a daemon restart, or after a freeze/EOF). A fresh
                // attach client only repaints the visible pane, so
                // reconstruct scrollback from tmux's surviving history
                // and hand it to the new client as its durable seed. Captured
                // WITHOUT the sessions lock held: `capture-pane` is a tmux
                // round trip, and the hot reuse path above must never wait
                // on it.
                let seed = self.capture_history(key).await;
                // Attach at the pane's CURRENT size, not a hardcoded
                // default. The surviving pane still carries the viewport
                // size its last client set, so matching it means tmux sees
                // no resize on attach — the inner program (Claude Code)
                // never runs its bottom-anchored incremental redraw and
                // can't leave a blank gap above the input box (#533).
                // A stale/absent size falls back to the default grid; the
                // widget's first-render resize still corrects it.
                let size = self
                    .pane_size(key)
                    .await
                    .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
                let mut map = self.sessions.lock().await;
                // Re-check under the lock: a concurrent subscribe may have
                // opened a client while we were capturing. If so, reuse it
                // and drop our seed rather than racing in a second client.
                if let Some(slot) = map.get(key).filter(|slot| !slot.client.is_finished()) {
                    slot.client.clone()
                } else {
                    // A finished slot may have appeared while capture-pane
                    // was in flight. Release its descriptor set before the
                    // replacement allocation for the same reason as the
                    // optimistic check above.
                    map.remove(key);
                    let (cols, rows) = size;
                    let size = PtySize {
                        cols,
                        rows,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    let slot = self.open_client(key, size, &seed)?;
                    let pty = slot.client.clone();
                    map.insert(key.into(), slot);
                    pty
                }
            };

            let mut sub = pty.subscribe().await;
            let replay = std::mem::take(&mut sub.replay);
            let replay_complete = sub.replay_complete;
            let last_seq = sub.last_seq;
            // Bounded bridge: a stalled subscriber drops chunks via
            // `try_send` instead of growing an unbounded backlog. The
            // `seq` gap + resync machinery recovers the consumer.
            let (tx, rx) = tokio::sync::mpsc::channel::<OutputChunk>(
                crate::backend::SUBSCRIPTION_CHANNEL_CAPACITY,
            );

            let pty_pump = pty.clone();
            let bridge_key = key.to_string();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = tx.closed() => break,
                        chunk = sub.live.recv() => match chunk {
                            Ok(c) => {
                                match tx.try_send(OutputChunk {
                                    seq: c.seq,
                                    bytes: c.bytes.to_vec(),
                                }) {
                                    Ok(()) => {}
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        // Drop the chunk — the spawn pump's
                                        // seq-gap detection (`spawn_handler`)
                                        // sees the hole on the next delivered
                                        // chunk and resyncs from the ring.
                                        tracing::debug!(
                                            key = %bridge_key,
                                            seq = c.seq,
                                            "tmux bridge channel full — dropping chunk"
                                        );
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        return;
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                // The bridge fell behind the PTY broadcast;
                                // `n` chunks vanished from the detection
                                // buffer. Surface it — silent holes here
                                // looked like a corrupted screen with no
                                // breadcrumb.
                                tracing::info!(
                                    key = %bridge_key,
                                    missed = n,
                                    "tmux bridge lagged behind PTY broadcast — chunks skipped"
                                );
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                        _ = pty_pump.wait_finished() => break,
                    }
                }
            });

            Ok(Subscription {
                replay,
                replay_complete,
                last_seq,
                live: rx,
            })
        })
    }

    fn is_alive<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let out = self.tmux_output(&["has-session", "-t", key]).await?;
            if out.status.success() {
                return Ok(true);
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("can't find session")
                || stderr.contains("no server running")
                || stderr.contains("no sessions")
            {
                return Ok(false);
            }
            Err(BackendError::Other(format!(
                "tmux has-session -t {key}: {}",
                stderr.trim()
            )))
        })
    }

    fn scrollback<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(Vec<u8>, u64)>, BackendError>> + Send + 'a>>
    {
        Box::pin(async move {
            // A pane with no retained history has nothing deeper than
            // what the client already rendered — capture-pane would
            // return just the visible screen, and adopting that as the
            // rebuilt grid REPLACES the client's (possibly deeper) local
            // scrollback with ~one screenful: the "scrollbar disappears
            // on the first scroll" regression. The alt screen is the
            // systematic zero-history case (now denied pane-side via
            // `alternate-screen off`, but panes that entered it under an
            // older server config stay there until their program leaves).
            match self.pane_history_state(key).await {
                Some((alternate_on, history_size)) if alternate_on || history_size == 0 => {
                    tracing::debug!(
                        key,
                        alternate_on,
                        history_size,
                        "scrollback fetch skipped — pane has no retained history"
                    );
                    return Ok(None);
                }
                _ => {}
            }
            // Same seed the restart/reattach path uses — tmux holds
            // `history-limit` lines for live sessions too, the client
            // just never read them before (#393).
            let seed = self.capture_history(key).await;
            if seed.is_empty() {
                return Ok(None);
            }
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.client.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            // Read the high-water mark AFTER capturing: a chunk that
            // lands in between is covered by neither the capture nor
            // the resumed live stream and simply waits for the inner
            // program's next repaint — dropping it beats double-feeding
            // bytes the capture already rendered.
            let last_seq = pty.snapshot_only().await.last_seq;
            Ok(Some((seed, last_seq)))
        })
    }

    fn wait_exit<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<i32>> + Send + 'a>> {
        Box::pin(async move {
            // This is the attach client's cached exit code. Recovery
            // checks `is_alive` before interpreting it as session death.
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key).map(|s| s.client.clone())?
            };
            pty.wait_exit().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config keeps the attach client off the alternate screen
    /// (smcup@/rmcup@ for the `xterm*` TERM the daemon PTY advertises)
    /// and leaves tmux's blanket mouse capture off so inner-program
    /// DECSET requests pass through to the client.
    #[test]
    fn conf_disables_mouse_and_alt_screen() {
        let conf = transparent_conf();
        assert!(conf.contains("set -g mouse off\n"));
        assert!(!conf.contains("mouse on"));
        assert!(conf.contains("set -g terminal-overrides ',xterm*:smcup@:rmcup@'\n"));
        assert!(conf.contains("set -g alternate-screen off\n"));
        // No copy-mode wheel bindings — the wheel never reaches tmux.
        assert!(!conf.contains("WheelUpPane"));
        // The transparent-client basics stay.
        assert!(conf.contains("set -g prefix None"));
        assert!(conf.contains("set -g status off"));
        assert!(conf.contains("set -g history-limit 10000"));
        assert!(conf.contains("unbind-key -a"));
        // Resize authority is pinned, not left to tmux's implicit default
        // (the `resize` impl's multi-client behavior depends on it).
        assert!(conf.contains("set -g window-size latest\n"));
    }

    /// The option pushes for a pre-existing tmux server mirror the
    /// config, so recovered sessions behave like fresh ones.
    #[test]
    fn server_option_cmds_match_conf() {
        let clipboard = [
            vec!["set-option", "-g", "set-clipboard", "on"],
            vec!["set-option", "-g", "allow-passthrough", "on"],
        ];
        let hyperlinks = vec![
            "set-option",
            "-as",
            "terminal-features",
            "xterm*:hyperlinks",
        ];
        let focus_events = vec!["set-option", "-g", "focus-events", "on"];
        let no_alt_screen = vec!["set-option", "-g", "alternate-screen", "off"];
        assert_eq!(
            server_option_cmds(),
            vec![
                vec!["set-option", "-g", "mouse", "off"],
                vec![
                    "set-option",
                    "-g",
                    "terminal-overrides",
                    ",xterm*:smcup@:rmcup@",
                ],
                hyperlinks,
                clipboard[0].clone(),
                clipboard[1].clone(),
                focus_events,
                no_alt_screen,
            ]
        );
    }

    /// The config enables clipboard passthrough so an inner program's
    /// OSC 52 reaches the host.
    #[test]
    fn conf_enables_clipboard_passthrough() {
        let conf = transparent_conf();
        assert!(conf.contains("set -g set-clipboard on\n"));
        assert!(conf.contains("set -g allow-passthrough on\n"));
    }

    /// The config advertises OSC 8 hyperlink support to the attach
    /// client. Without this tmux strips hyperlinks (the forced
    /// `TERM=xterm-256color` has no `Hls` capability), so right-click on
    /// an agent's titled link finds no URI to open. Regression for the
    /// "right-click never opens URLs under the tmux backend" report.
    #[test]
    fn conf_advertises_hyperlinks() {
        assert!(transparent_conf().contains("set -as terminal-features 'xterm*:hyperlinks'\n"));
    }

    /// Both config and server-option paths enable focus-events
    /// so agents (e.g. Claude Code) running inside the tmux backend stop
    /// nagging "focus-events off". Regression for the focus-events warning.
    #[test]
    fn both_paths_enable_focus_events() {
        assert!(transparent_conf().contains("set -g focus-events on\n"));
        assert!(server_option_cmds().contains(&vec!["set-option", "-g", "focus-events", "on"]));
    }

    /// `with_socket` drops the config to disk and the
    /// attach argv routes every client through it via `-f`.
    #[test]
    fn with_socket_writes_conf_and_attach_uses_it() {
        let socket = format!("lazybox-test-conf-{}", std::process::id());
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let on_disk = std::fs::read_to_string(&backend.config_path).expect("conf readable");
        assert_eq!(on_disk, transparent_conf());
        assert!(on_disk.contains("smcup@:rmcup@"));

        let argv = backend.attach_argv("lazybox-some-key");
        assert_eq!(argv[0], "tmux");
        let f_pos = argv.iter().position(|a| a == "-f").expect("-f present");
        assert_eq!(argv[f_pos + 1], backend.config_path.to_string_lossy());
        assert!(argv.windows(2).any(|w| w[0] == "-L" && w[1] == socket));
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-t" && w[1] == "lazybox-some-key")
        );

        let _ = std::fs::remove_file(&backend.config_path);
    }

    /// Version banners across the shapes tmux actually emits. The gate
    /// must reject anything confidently below [`MIN_TMUX_VERSION`] (an
    /// unknown conf option swaps every attach into tmux's config-error
    /// view — dead sessions) and admit dev/vendor builds it can't parse.
    #[test]
    fn tmux_version_gate() {
        // Release banners.
        assert_eq!(parse_tmux_version("tmux 3.2a"), Some((3, 2)));
        assert_eq!(parse_tmux_version("tmux 3.3"), Some((3, 3)));
        assert_eq!(parse_tmux_version("tmux 3.5a"), Some((3, 5)));
        // Development banner.
        assert_eq!(parse_tmux_version("tmux next-3.4"), Some((3, 4)));
        // No dotted version — OpenBSD tracks head; treated as modern.
        assert_eq!(parse_tmux_version("tmux openbsd-7"), None);
        assert_eq!(parse_tmux_version("tmux"), None);

        assert!(parse_tmux_version("tmux 3.2a").unwrap() < MIN_TMUX_VERSION);
        assert!(parse_tmux_version("tmux 3.3a").unwrap() >= MIN_TMUX_VERSION);
        assert!(parse_tmux_version("tmux 4.0").unwrap() >= MIN_TMUX_VERSION);
    }

    /// The spawn's `new-session` argv seeds `COLORTERM=truecolor` into
    /// the tmux session environment. `default-terminal` covers `TERM`,
    /// but tmux doesn't propagate the attach client's `COLORTERM`, so
    /// without the `-e` the inner agent fails its truecolor probe and
    /// renders without colors. Regression for #421 (Codex monochrome
    /// under the tmux backend).
    #[test]
    fn new_session_args_seed_colorterm_truecolor() {
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let argv = vec!["codex".to_string(), "--flag".to_string()];
        let args = new_session_args("lazybox-codex-1", None, &env, &argv);

        let e_vals: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-e")
            .map(|w| w[1].as_str())
            .collect();
        assert!(
            e_vals.contains(&"COLORTERM=truecolor"),
            "COLORTERM must be seeded into the session env: {args:?}"
        );
        // Caller env still rides along, and ours is seeded after it so
        // the forced value wins (mirrors DaemonPty forcing TERM last).
        assert_eq!(e_vals, ["FOO=bar", "COLORTERM=truecolor"]);
        // All `-e` pairs sit before the `--` separator, i.e. they are
        // tmux flags, not command tokens.
        let sep = args.iter().position(|a| a == "--").expect("-- present");
        let last_e = args.iter().rposition(|a| a == "-e").expect("-e present");
        assert!(last_e + 1 < sep, "env flags precede the -- separator");
        assert_eq!(&args[sep + 1..], &argv[..], "argv follows the separator");
    }

    /// capture-pane joins lines with bare `\n`; the seed must carriage-
    /// return each line (LNM is off in the client VT) and drop the
    /// trailing newline so the cursor parks at the end of the last
    /// history line, where the live attach repaint picks up.
    #[test]
    fn normalize_capture_crlf_and_trailing_newline() {
        assert_eq!(
            normalize_capture(b"one\ntwo\nthree\n"),
            b"one\r\ntwo\r\nthree"
        );
        // Escape sequences from `-e` pass through untouched.
        assert_eq!(
            normalize_capture(b"\x1b[31mred\x1b[0m\nplain\n"),
            b"\x1b[31mred\x1b[0m\r\nplain"
        );
        // No trailing newline → nothing stripped.
        assert_eq!(normalize_capture(b"tail"), b"tail");
        assert_eq!(normalize_capture(b""), b"");
    }

    /// capture-pane pads its output to the full pane grid, so a short
    /// session's capture ends in a run of blank rows. Those are padding,
    /// not scrollback — seeding them verbatim injected the "random extra
    /// empty lines after restart / move-session" (#442). The seed must end
    /// at the last row with real content, and interior blanks (genuine
    /// output) must survive.
    #[test]
    fn normalize_capture_trims_trailing_padding_blanks() {
        // Content rows followed by grid-padding blanks.
        assert_eq!(
            normalize_capture(b"one\ntwo\nthree\n\n\n\n"),
            b"one\r\ntwo\r\nthree"
        );
        // Whitespace-only padding rows are trimmed too (defensive:
        // capture-pane strips trailing spaces, but a run must never leak).
        assert_eq!(normalize_capture(b"content\n   \n\t\n"), b"content");
        // An all-blank capture reduces to nothing — no seed to inject.
        assert_eq!(normalize_capture(b"\n\n\n"), b"");
        // Interior blank lines are genuine output; only the trailing
        // padding run is trimmed.
        assert_eq!(
            normalize_capture(b"top\n\nmiddle\n\n\n"),
            b"top\r\n\r\nmiddle"
        );
        // A trailing row that is only `-e` escapes around blank space is
        // grid padding cleared under a style — the escapes are skipped and
        // the row trims like any other blank (a bare SGR reset, and a
        // background-colored run of spaces).
        assert_eq!(normalize_capture(b"body\n\x1b[0m\n"), b"body");
        assert_eq!(
            normalize_capture(b"body\n\x1b[48;5;236m   \x1b[0m\n"),
            b"body"
        );
        // A trailing row with a real glyph is content and survives, escape
        // styling and all — that printable char is what protects it.
        assert_eq!(
            normalize_capture(b"a\n\x1b[31mred\x1b[0m\n"),
            b"a\r\n\x1b[31mred\x1b[0m"
        );
    }

    /// The `-S` capture depth and the conf's `history-limit` come from
    /// the same constant, so seeding can never under-read the history
    /// tmux was told to keep.
    #[test]
    fn capture_depth_matches_history_limit() {
        assert!(transparent_conf().contains(&format!("set -g history-limit {HISTORY_LIMIT}\n")));
    }
}
