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

/// tmux client config: prefix off (so Ctrl-B reaches the agent), no
/// key bindings (so nothing intercepts), no status bar (so output
/// isn't framed). Set as a string and dropped to a temp file at
/// `TmuxBackend::new` time so we don't depend on the user's `~/.tmux.conf`.
///
/// `mouse on` + `history-limit 10000` make the wheel scroll inside a
/// shell pane work: tmux owns the alt-screen here (that's why
/// libghostty reports alternate), so a Delta scroll on libghostty's
/// own grid is a no-op. With mouse on, tmux receives the wheel via
/// our encoded SGR sequence and enters copy-mode automatically; for
/// inner programs that themselves request mouse tracking
/// (claude / vim / less) tmux forwards the event through to them
/// untouched, so they keep handling scroll natively.
const TMUX_TRANSPARENT_CONF: &str = "\
set -g prefix None
set -g status off
set -g mouse on
set -g history-limit 10000
set -g default-terminal \"xterm-256color\"
set -g escape-time 0
set -g mode-style \"fg=default,bg=default\"
set -g message-style \"fg=default,bg=default\"
unbind-key -a
# Wheel scrolls ONE line per notch. macOS trackpad already fires
# ~30 events per gesture, so 30 × 1 = ~30 lines per swipe — about
# what a native terminal feels like. The earlier `-N 10` bump
# (intended to compensate for slow-feeling scroll) compounded with
# the per-gesture event count to give ~300 lines per swipe, which
# the user described as \"moving 10 lines at a time\" — each
# trackpad tick teleported.
bind-key -T copy-mode-vi WheelUpPane send-keys -X scroll-up
bind-key -T copy-mode-vi WheelDownPane send-keys -X scroll-down
bind-key -T copy-mode WheelUpPane send-keys -X scroll-up
bind-key -T copy-mode WheelDownPane send-keys -X scroll-down
";

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
    sessions: Mutex<HashMap<String, Slot>>,
    next_key: AtomicU64,
}

impl TmuxBackend {
    /// Probe for tmux on PATH and write the transparent config. Returns
    /// `None` when tmux isn't usable on this machine — callers fall
    /// back to `RawPtyBackend`.
    pub fn detect() -> Option<Self> {
        let out = Command::new("tmux").arg("-V").output().ok()?;
        if !out.status.success() {
            tracing::debug!("tmux -V failed; tmux backend unavailable");
            return None;
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        tracing::info!("tmux backend available: {version}");
        // Profile-aware socket name. Default profile resolves to
        // "lazybox" — backward compatible with running sessions; a
        // dev profile (`LAZYBOX_HOME=~/.lazybox-dev`) gets "lazybox-dev"
        // so two lazybox daemons don't share session state.
        let socket = lazybox_core::paths::tmux_socket_name();
        Self::with_socket(&socket).ok()
    }

    /// Build a backend pinned to a specific tmux socket name. Useful
    /// for tests so concurrent runs don't share state.
    pub fn with_socket(socket: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join("lazybox-tmux");
        std::fs::create_dir_all(&dir)?;
        let config_path = dir.join(format!("{socket}.conf"));
        std::fs::write(&config_path, TMUX_TRANSPARENT_CONF)?;
        Ok(Self {
            socket: socket.into(),
            config_path,
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
            let _ = std::fs::write(&self.config_path, TMUX_TRANSPARENT_CONF);
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
    fn open_client(&self, key: &str, size: PtySize) -> Result<Slot, BackendError> {
        let argv = self.attach_argv(key);
        let pty = DaemonPty::spawn(&argv, size, None, Vec::new())
            .map_err(|e| BackendError::Spawn(format!("tmux attach: {e}")))?;
        Ok(Slot {
            client: Arc::new(pty),
        })
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

            // Now open the attaching client. If this fails the tmux
            // session is orphaned; tear it down so we don't leak.
            let size = PtySize {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                pixel_width: 0,
                pixel_height: 0,
            };
            let slot = match self.open_client(&key, size) {
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
            // Resizing the attached client makes tmux resize the pane
            // automatically. We also nudge the tmux session itself in
            // case other clients are attached at different sizes.
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
            // Best-effort window resize. Failing this is fine — the
            // PTY-level resize already triggered tmux internally.
            let _ = self
                .tmux(&["refresh-client", "-t", key, "-C", &format!("{cols},{rows}")])
                .await;
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
            // haven't bound a client yet, open one lazily.
            let pty = {
                let mut map = self.sessions.lock().await;
                if let Some(slot) = map.get(key) {
                    slot.client.clone()
                } else {
                    let size = PtySize {
                        cols: DEFAULT_COLS,
                        rows: DEFAULT_ROWS,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    let slot = self.open_client(key, size)?;
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

    fn freeze<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // Detach every client from this tmux session. Inner
            // processes (claude, shell, …) keep running paused on
            // their next read since nothing's reading their stdin;
            // we'll re-attach in `resume`. Best-effort: silently
            // succeed if tmux complains (no clients to detach is a
            // success state for our purpose).
            let fut = tokio::process::Command::new("tmux")
                .arg("-L")
                .arg(&self.socket)
                .arg("detach-client")
                .arg("-s")
                .arg(key)
                .arg("-a")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .output();
            if tokio::time::timeout(TMUX_TIMEOUT, fut).await.is_err() {
                return Err(BackendError::Other(format!(
                    "tmux detach-client -s {key} timed out after {}s",
                    TMUX_TIMEOUT.as_secs()
                )));
            }
            Ok(())
        })
    }

    fn resume<'a>(
        &'a self,
        _key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        // Re-attach happens implicitly on the next subscribe — lazybox
        // already has the per-client `attach-session` flow wired in
        // `subscribe`. So `resume` is a deliberate no-op: the caller
        // either re-subscribes (live client) or leaves the session
        // detached for later attach.
        Box::pin(async { Ok(()) })
    }
}
