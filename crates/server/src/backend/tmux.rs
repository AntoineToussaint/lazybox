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

/// Clipboard passthrough options, independent of the scrollback flavor.
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
/// Two flavors, keyed on `terminal.native_scrollback`:
///
/// - **native scrollback (default)** — `mouse off` plus the
///   smcup@/rmcup@ override above. The lazybox client keeps the
///   relayed output on libghostty's PRIMARY screen, so wheel /
///   Shift-PageUp scroll the local 10k-line scrollback instantly with
///   no daemon round trip. tmux's `mouse off` also means a DECSET
///   mouse-mode request from the INNER program (vim, htop, …) passes
///   through to the client untouched, so `is_mouse_tracking()` on the
///   client reflects the inner app and the wheel routes to it when —
///   and only when — it asked for the mouse.
///
/// - **legacy** — `mouse on`: tmux owns the alt-screen, receives the
///   wheel via our encoded SGR sequence and enters copy-mode
///   automatically, scrolling its own history one line per notch
///   (a daemon round trip + pane repaint per notch).
///
/// Every option here may assume [`MIN_TMUX_VERSION`] — `detect()`
/// refuses older tmux, because a single unknown option in this conf
/// breaks every attach (see the constant's doc). Raising an option's
/// floor means raising `MIN_TMUX_VERSION` with it.
fn transparent_conf(native_scrollback: bool) -> String {
    let mut conf = String::from(
        "set -g prefix None\n\
         set -g status off\n",
    );
    if native_scrollback {
        conf.push_str("set -g mouse off\n");
    } else {
        conf.push_str("set -g mouse on\n");
    }
    conf.push_str(&format!("set -g history-limit {HISTORY_LIMIT}\n"));
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
    if native_scrollback {
        conf.push_str("set -g terminal-overrides '");
        conf.push_str(SCROLLBACK_TERMINAL_OVERRIDES);
        conf.push_str("'\n");
    }
    conf.push_str("unbind-key -a\n");
    if !native_scrollback {
        conf.push_str(
            "# Wheel scrolls ONE line per notch. macOS trackpad already fires\n\
             # ~30 events per gesture, so 30 × 1 = ~30 lines per swipe — about\n\
             # what a native terminal feels like. The earlier `-N 10` bump\n\
             # (intended to compensate for slow-feeling scroll) compounded with\n\
             # the per-gesture event count to give ~300 lines per swipe, which\n\
             # the user described as \"moving 10 lines at a time\" — each\n\
             # trackpad tick teleported.\n\
             bind-key -T copy-mode-vi WheelUpPane send-keys -X scroll-up\n\
             bind-key -T copy-mode-vi WheelDownPane send-keys -X scroll-down\n\
             bind-key -T copy-mode WheelUpPane send-keys -X scroll-up\n\
             bind-key -T copy-mode WheelDownPane send-keys -X scroll-down\n",
        );
    }
    conf
}

/// Turn `tmux capture-pane -p` output into replay-ring bytes.
///
/// capture-pane separates pane lines with a bare `\n`. The reattaching
/// client's VT parser runs with LNM off, where `\n` is a plain line feed
/// that moves the cursor down WITHOUT returning to column 0 — feeding the
/// capture verbatim would staircase every line. So each `\n` becomes
/// `\r\n`. The trailing newline is dropped so the cursor lands at the end
/// of the last history line, exactly where tmux's live attach repaint
/// resumes, stitching seeded history to the live screen without a blank
/// row between them.
fn normalize_capture(stdout: &[u8]) -> Vec<u8> {
    let trimmed = stdout.strip_suffix(b"\n").unwrap_or(stdout);
    let mut seed = Vec::with_capacity(trimmed.len() + 16);
    for &b in trimmed {
        if b == b'\n' {
            seed.push(b'\r');
        }
        seed.push(b);
    }
    seed
}

/// `set-option` invocations that bring an ALREADY-RUNNING tmux server
/// (started by an older lazybox with the other conf flavor) in line
/// with `native_scrollback`. The `-f` conf only applies at tmux server
/// start, so sessions that survived a lazybox upgrade would otherwise
/// keep the old mouse / alt-screen behavior forever. Applied once per
/// backend process, before the first attach, so re-attached clients
/// pick the overrides up (terminal-overrides is consulted at client
/// attach time).
fn server_option_cmds(native_scrollback: bool) -> Vec<Vec<&'static str>> {
    // Clipboard passthrough is independent of the scrollback flavor, so
    // an already-running server picks it up either way.
    let clipboard = [
        vec!["set-option", "-g", "set-clipboard", "on"],
        vec!["set-option", "-g", "allow-passthrough", "on"],
    ];
    // Agents (e.g. Claude Code) nag when they detect tmux with
    // focus-events off. We own the config, so enable it for both fresh
    // and recovered servers.
    let focus_events = vec!["set-option", "-g", "focus-events", "on"];
    // `terminal-features` is independent of the scrollback flavor — an
    // already-running server must learn the client speaks OSC 8 either
    // way, else surviving sessions keep stripping hyperlinks.
    let hyperlinks = vec![
        "set-option",
        "-as",
        "terminal-features",
        HYPERLINK_TERMINAL_FEATURES_VALUE,
    ];
    let mut cmds = if native_scrollback {
        vec![
            vec!["set-option", "-g", "mouse", "off"],
            vec![
                "set-option",
                "-g",
                "terminal-overrides",
                SCROLLBACK_TERMINAL_OVERRIDES,
            ],
            hyperlinks,
        ]
    } else {
        vec![
            vec!["set-option", "-g", "mouse", "on"],
            vec!["set-option", "-gu", "terminal-overrides"],
            hyperlinks,
        ]
    };
    cmds.extend(clipboard);
    cmds.push(focus_events);
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
    /// Rendered conf contents — `transparent_conf(native_scrollback)`.
    /// Kept so the self-heal rewrite in `tmux()` reproduces the same
    /// flavor the backend was built with.
    conf: String,
    /// `terminal.native_scrollback` — local client scrollback (mouse
    /// off + no alt-screen on the attach client) vs legacy tmux
    /// copy-mode scrolling.
    native_scrollback: bool,
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
        // `terminal.native_scrollback` from the user config; the
        // load failure path (corrupt YAML) falls back to the default
        // (on) rather than silently flipping the scroll model.
        let native_scrollback = lazybox_config::Config::load()
            .map(|c| c.terminal.native_scrollback)
            .unwrap_or(true);
        Self::with_socket_scrollback(&socket, native_scrollback).ok()
    }

    /// Build a backend pinned to a specific tmux socket name with the
    /// default (native) scrollback mode. Useful for tests so
    /// concurrent runs don't share state.
    pub fn with_socket(socket: &str) -> std::io::Result<Self> {
        Self::with_socket_scrollback(socket, true)
    }

    /// `with_socket` with an explicit `terminal.native_scrollback`
    /// value.
    pub fn with_socket_scrollback(socket: &str, native_scrollback: bool) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join("lazybox-tmux");
        std::fs::create_dir_all(&dir)?;
        let config_path = dir.join(format!("{socket}.conf"));
        let conf = transparent_conf(native_scrollback);
        std::fs::write(&config_path, &conf)?;
        Ok(Self {
            socket: socket.into(),
            config_path,
            conf,
            native_scrollback,
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

    /// Run `tmux -L <socket> -f <config> ...args`. Captures stdout +
    /// stderr; returns a BackendError on non-zero exit.
    ///
    /// **Async**: `tokio::process::Command` rather than the sync std
    /// version. The backend's trait methods are wrapped in async
    /// futures, and a sync `output()` here would block the entire
    /// tokio runtime for the duration of every tmux invocation —
    /// which can be 100ms+ during server startup. Every other task
    /// (TUI render, IPC pumps, polling) would freeze in lockstep.
    async fn tmux(&self, args: &[&str]) -> Result<std::process::Output, BackendError> {
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
    /// (old conf flavor) keeps its options across our restarts — this
    /// pushes the current flavor onto it explicitly so both freshly
    /// spawned and recovered sessions behave the same. Best-effort:
    /// a failure degrades scrolling, it must not block the spawn.
    async fn ensure_server_options(&self) {
        // `get_or_init` makes concurrent callers WAIT for the first
        // one to finish setting the options, instead of attaching
        // against a half-configured server.
        self.options_applied
            .get_or_init(|| async {
                for cmd in server_option_cmds(self.native_scrollback) {
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
    /// session's — `wait_exit` polls tmux directly for that.
    ///
    /// `seed` pre-loads the DaemonPty's replay ring with reconstructed
    /// scrollback (see `capture_history`). A plain `tmux attach` only
    /// repaints the visible pane, so without the seed a client that
    /// reattaches to a session which survived a daemon restart has one
    /// screenful and nothing to scroll back through.
    fn open_client(&self, key: &str, size: PtySize, seed: &[u8]) -> Result<Slot, BackendError> {
        let argv = self.attach_argv(key);
        let pty = DaemonPty::spawn(&argv, size, None, Vec::new(), seed)
            .map_err(|e| BackendError::Spawn(format!("tmux attach: {e}")))?;
        Ok(Slot {
            client: Arc::new(pty),
        })
    }

    /// Reconstruct a reattaching client's scrollback from tmux's own
    /// history. The daemon's replay ring lives in memory and dies with
    /// the daemon; tmux, however, keeps `history-limit` lines per pane
    /// across restarts. `capture-pane -e -S -<limit>` dumps that history
    /// (styled, via `-e`) down to the current bottom line, which we feed
    /// into the ring ahead of the live attach bytes so the client
    /// rebuilds the full scrollback instead of a single repainted screen.
    ///
    /// Best-effort: any failure returns an empty seed and the client
    /// simply starts from the live repaint, exactly as before this fix.
    async fn capture_history(&self, key: &str) -> Vec<u8> {
        let start = format!("-{HISTORY_LIMIT}");
        let out = match self
            .tmux(&["capture-pane", "-p", "-e", "-S", &start, "-t", key])
            .await
        {
            Ok(out) => out.stdout,
            Err(e) => {
                tracing::warn!(key, "tmux capture-pane for scrollback seed failed: {e}");
                return Vec::new();
            }
        };
        normalize_capture(&out)
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

            // Build `tmux new-session -d -s <key> -x <cols> -y <rows> [-c <cwd>] -- <argv...>`.
            // Detached session — we attach our own client below.
            let cols = DEFAULT_COLS.to_string();
            let rows = DEFAULT_ROWS.to_string();
            let mut cmd_args: Vec<String> = vec![
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                key.clone(),
                "-x".into(),
                cols,
                "-y".into(),
                rows,
            ];
            if let Some(dir) = cwd {
                cmd_args.push("-c".into());
                cmd_args.push(dir.to_string_lossy().into_owned());
            }
            for (k, v) in env {
                cmd_args.push("-e".into());
                cmd_args.push(format!("{k}={v}"));
            }
            // `--` separator so argv elements starting with `-` are
            // treated as command tokens, not tmux flags.
            cmd_args.push("--".into());
            cmd_args.extend(argv.iter().cloned());

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
            // Drop our slot first so subsequent ops are NotFound.
            let slot = self.sessions.lock().await.remove(key);
            if let Some(slot) = slot {
                slot.client.kill();
            }
            // Kill the tmux session. Idempotent: if the session is
            // already gone tmux exits non-zero — we ignore that. Real
            // failures (tmux not on PATH) will already have surfaced
            // at spawn time.
            let _ = self.tmux(&["kill-session", "-t", key]).await;
            Ok(())
        })
    }

    fn list<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // Two sources of truth: the in-memory map (sessions we
            // spawned) and `tmux list-sessions` (what tmux currently
            // sees, including any survivors of a prior lazybox run).
            // We return the union so restart recovery works even
            // before the server has rebound clients to those keys.
            let mut keys: Vec<String> = self.sessions.lock().await.keys().cloned().collect();
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
                    if !name.is_empty() && !keys.contains(&name) {
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
    ) -> Pin<Box<dyn Future<Output = Result<(Vec<u8>, u64), BackendError>> + Send + 'a>> {
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
            // push the current option flavor before attaching so they
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
                let map = self.sessions.lock().await;
                map.get(key)
                    .filter(|slot| !slot.client.is_finished())
                    .map(|slot| slot.client.clone())
            };
            let pty = if let Some(pty) = cached {
                pty
            } else {
                // No live client — this is the reattach path (recovery
                // after a daemon restart, or after a freeze/EOF). A fresh
                // attach client only repaints the visible pane, so
                // reconstruct scrollback from tmux's surviving history
                // and seed it into the new client's replay ring. Captured
                // WITHOUT the sessions lock held: `capture-pane` is a tmux
                // round trip, and the hot reuse path above must never wait
                // on it.
                let seed = self.capture_history(key).await;
                let mut map = self.sessions.lock().await;
                // Re-check under the lock: a concurrent subscribe may have
                // opened a client while we were capturing. If so, reuse it
                // and drop our seed rather than racing in a second client.
                if let Some(slot) = map.get(key).filter(|slot| !slot.client.is_finished()) {
                    slot.client.clone()
                } else {
                    let size = PtySize {
                        cols: DEFAULT_COLS,
                        rows: DEFAULT_ROWS,
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
                        chunk = sub.live.recv() => match chunk {
                            Ok(c) => {
                                match tx.try_send(OutputChunk {
                                    seq: c.seq,
                                    bytes: c.bytes.to_vec(),
                                }) {
                                    Ok(()) => {}
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        // Drop the chunk — the consumer's
                                        // seq-gap detection schedules a
                                        // resync from the replay ring.
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
                last_seq,
                live: rx,
            })
        })
    }

    fn wait_exit<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<i32>> + Send + 'a>> {
        Box::pin(async move {
            // The attach client exits when its tmux session ends —
            // either because the inner process exited, or because
            // someone called `kill_session`. DaemonPty caches the
            // exit code, so this is safe to call repeatedly.
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

    /// Native mode keeps the attach client off the alternate screen
    /// (smcup@/rmcup@ for the `xterm*` TERM the daemon PTY advertises)
    /// and leaves tmux's blanket mouse capture off so inner-program
    /// DECSET requests pass through to the client.
    #[test]
    fn native_conf_disables_mouse_and_alt_screen() {
        let conf = transparent_conf(true);
        assert!(conf.contains("set -g mouse off\n"));
        assert!(!conf.contains("mouse on"));
        assert!(conf.contains("set -g terminal-overrides ',xterm*:smcup@:rmcup@'\n"));
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

    /// Legacy mode (`terminal.native_scrollback: false`) reproduces the
    /// previous behavior byte-for-byte in spirit: tmux owns the mouse,
    /// the alt-screen, and copy-mode wheel scrolling.
    #[test]
    fn legacy_conf_keeps_tmux_mouse_and_copy_mode() {
        let conf = transparent_conf(false);
        assert!(conf.contains("set -g mouse on\n"));
        assert!(!conf.contains("terminal-overrides"));
        assert!(conf.contains("bind-key -T copy-mode-vi WheelUpPane send-keys -X scroll-up\n"));
        assert!(conf.contains("bind-key -T copy-mode WheelDownPane send-keys -X scroll-down\n"));
        assert!(conf.contains("unbind-key -a"));
    }

    /// The option pushes for a pre-existing tmux server mirror the
    /// conf flavors, so recovered sessions behave like fresh ones.
    #[test]
    fn server_option_cmds_match_conf_flavors() {
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
        let native = server_option_cmds(true);
        assert_eq!(
            native,
            vec![
                vec!["set-option", "-g", "mouse", "off"],
                vec![
                    "set-option",
                    "-g",
                    "terminal-overrides",
                    ",xterm*:smcup@:rmcup@",
                ],
                hyperlinks.clone(),
                clipboard[0].clone(),
                clipboard[1].clone(),
                focus_events.clone(),
            ]
        );
        let legacy = server_option_cmds(false);
        assert_eq!(
            legacy,
            vec![
                vec!["set-option", "-g", "mouse", "on"],
                vec!["set-option", "-gu", "terminal-overrides"],
                hyperlinks,
                clipboard[0].clone(),
                clipboard[1].clone(),
                focus_events,
            ]
        );
    }

    /// Both conf flavors enable clipboard passthrough so an inner
    /// program's OSC 52 reaches the host. Regression for "copy from the
    /// agent silently fails under the tmux backend".
    #[test]
    fn both_conf_flavors_enable_clipboard_passthrough() {
        for native in [true, false] {
            let conf = transparent_conf(native);
            assert!(
                conf.contains("set -g set-clipboard on\n"),
                "native={native}"
            );
            assert!(
                conf.contains("set -g allow-passthrough on\n"),
                "native={native}"
            );
        }
    }

    /// Both conf flavors advertise OSC 8 hyperlink support to the attach
    /// client. Without this tmux strips hyperlinks (the forced
    /// `TERM=xterm-256color` has no `Hls` capability), so right-click on
    /// an agent's titled link finds no URI to open. Regression for the
    /// "right-click never opens URLs under the tmux backend" report.
    #[test]
    fn both_conf_flavors_advertise_hyperlinks() {
        assert!(transparent_conf(true).contains("set -as terminal-features 'xterm*:hyperlinks'\n"));
        assert!(
            transparent_conf(false).contains("set -as terminal-features 'xterm*:hyperlinks'\n")
        );
    }

    /// Both conf flavors and both server-option paths enable focus-events
    /// so agents (e.g. Claude Code) running inside the tmux backend stop
    /// nagging "focus-events off". Regression for the focus-events warning.
    #[test]
    fn both_paths_enable_focus_events() {
        for native in [true, false] {
            assert!(
                transparent_conf(native).contains("set -g focus-events on\n"),
                "native={native}"
            );
            assert!(
                server_option_cmds(native).contains(&vec![
                    "set-option",
                    "-g",
                    "focus-events",
                    "on"
                ]),
                "native={native}"
            );
        }
    }

    /// `with_socket` drops the native-flavor conf to disk and the
    /// attach argv routes every client through it via `-f`.
    #[test]
    fn with_socket_writes_native_conf_and_attach_uses_it() {
        let socket = format!("lazybox-test-conf-{}", std::process::id());
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let on_disk = std::fs::read_to_string(&backend.config_path).expect("conf readable");
        assert_eq!(on_disk, transparent_conf(true));
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

    /// The `-S` capture depth and the conf's `history-limit` come from
    /// the same constant, so seeding can never under-read the history
    /// tmux was told to keep.
    #[test]
    fn capture_depth_matches_history_limit() {
        assert!(
            transparent_conf(true).contains(&format!("set -g history-limit {HISTORY_LIMIT}\n"))
        );
    }

    /// The escape hatch writes the legacy conf.
    #[test]
    fn with_socket_scrollback_off_writes_legacy_conf() {
        let socket = format!("lazybox-test-legacy-{}", std::process::id());
        let backend = TmuxBackend::with_socket_scrollback(&socket, false).expect("conf written");
        let on_disk = std::fs::read_to_string(&backend.config_path).expect("conf readable");
        assert_eq!(on_disk, transparent_conf(false));
        assert!(on_disk.contains("set -g mouse on"));
        assert!(!on_disk.contains("terminal-overrides"));

        let _ = std::fs::remove_file(&backend.config_path);
    }
}
