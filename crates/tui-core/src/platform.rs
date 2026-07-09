//! Cross-platform shims for the few OS-specific bits lazybox needs.
//!
//! Lazybox is Unix-first today (macOS + Linux) but the long-term plan
//! is a Windows port. Rather than scatter `cfg(unix)` blocks across
//! `main.rs`, `realm::model`, and `lazybox-server::lifecycle`, the
//! platform-touching primitives live here. Each function has a unix
//! impl that does the real thing and a windows stub that returns an
//! error or `pending()`. When the Windows port lands, fill in the
//! windows arms and the rest of the code compiles unchanged.
//!
//! ## What's wrapped
//!
//! - [`redirect_stderr_to_file`] — point fd 2 at a file (`dup2` on
//!   unix; Windows would use `SetStdHandle` + `ReOpenFile`).
//! - [`detach_child_process`] — set up a `std::process::Command` so
//!   the spawned child outlives the parent (`setsid` on unix; Windows
//!   would use `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS`).
//! - [`wait_for_shutdown_signal`] — async wait for SIGTERM / SIGINT
//!   (or Ctrl-Break on Windows). Resolves once.

/// Redirect process stderr (fd 2) to the given open file. Best-effort
/// — failures are silently ignored; the caller already has a fallback
/// (the tracing layer also writes to the file directly).
///
/// **Why:** native logging from below the Rust layer (libghostty-vt's
/// Zig `log.warn`, libgit2 stderr, agent CLIs that write to fd 2)
/// paints directly onto the user's terminal otherwise, corrupting
/// the alternate-screen frame ratatui just drew. Routing fd 2 into
/// `/tmp/lazybox.log` keeps the screen clean.
pub fn redirect_stderr_to_file(file: &std::fs::File) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // Safety: `dup2` is sound here — `file.as_raw_fd()` is a
        // valid fd we own, fd 2 is always valid, and the call
        // doesn't expose any pointers. Done before any TUI subsystem
        // starts.
        unsafe {
            let _ = libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        }
    }
    #[cfg(windows)]
    {
        // TODO(windows): SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle())
        let _ = file;
    }
}

/// Detach a child `Command` from the parent's session group so the
/// child survives the parent process exiting. Used when launching
/// external editors / browsers so they outlive lazybox.
///
/// On unix: `setsid()` via `pre_exec`. On Windows: `CREATE_NEW_PROCESS_GROUP`
/// + `DETACHED_PROCESS` flags (TODO).
pub fn detach_child_process(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Safety: `setsid()` only mutates the calling process's
        // session-id — no pointer hazards, no Rust-side state.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        // TODO(windows): CommandExt::creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        let _ = cmd;
    }
}

/// Which delivery mechanism carries desktop notifications. Configured
/// via `attention.notifier` in `~/.lazybox/config.yaml` and armed by
/// the binary at startup with [`set_notifier_backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifierBackend {
    /// Pick per environment: the subprocess path when a local helper
    /// can reach the user (it's verifiable and immune to terminal
    /// OSC quirks), the OSC escape path over SSH (the only surface
    /// that reaches the *local* machine).
    Auto,
    /// Always the terminal's OSC notification sequence.
    Osc,
    /// Always the subprocess helpers (`terminal-notifier` /
    /// `osascript` / `notify-send`).
    Subprocess,
}

const BACKEND_DISARMED: u8 = 0;
const BACKEND_AUTO: u8 = 1;
const BACKEND_OSC: u8 = 2;
const BACKEND_SUBPROCESS: u8 = 3;

/// Disarmed until the binary opts in. Library and test code drives
/// the same event paths that fire notifications (`cargo test` pushes
/// `AgentState::InputNeeded` through the sidebar), and an un-armed
/// default keeps those runs from spawning real OS banners.
static BACKEND: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(BACKEND_DISARMED);

/// Arm desktop notifications with the configured backend. Called once
/// at startup by the `lazybox` binary; until then [`notify_user`] is
/// a logged no-op.
pub fn set_notifier_backend(backend: NotifierBackend) {
    let v = match backend {
        NotifierBackend::Auto => BACKEND_AUTO,
        NotifierBackend::Osc => BACKEND_OSC,
        NotifierBackend::Subprocess => BACKEND_SUBPROCESS,
    };
    BACKEND.store(v, std::sync::atomic::Ordering::Relaxed);
}

fn notifier_backend() -> Option<NotifierBackend> {
    match BACKEND.load(std::sync::atomic::Ordering::Relaxed) {
        BACKEND_AUTO => Some(NotifierBackend::Auto),
        BACKEND_OSC => Some(NotifierBackend::Osc),
        BACKEND_SUBPROCESS => Some(NotifierBackend::Subprocess),
        _ => None,
    }
}

/// Resolved delivery path for one notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Osc(crate::notify::OscNotifier),
    Subprocess,
}

/// Pick the delivery path. Pure so the policy is unit-testable.
///
/// `Auto` prefers the subprocess helpers whenever they can reach the
/// user: locally they're verifiable (exit status lands in the log)
/// and immune to terminal/OSC quirks — an unhandled or corrupted
/// escape sequence means a silently lost banner at best, literal
/// junk on screen at worst. Over SSH the helpers would fire on the
/// remote host where no one is looking, so the terminal's OSC
/// surface (which the local emulator renders) wins there.
fn resolve_route(
    backend: NotifierBackend,
    osc: Option<crate::notify::OscNotifier>,
    remote: bool,
    subprocess_available: bool,
) -> Route {
    use crate::notify::OscNotifier;
    match backend {
        NotifierBackend::Subprocess => Route::Subprocess,
        // Explicit `osc` trusts the user over detection: `$TERM_PROGRAM`
        // often doesn't survive an SSH hop, so an unrecognized terminal
        // gets the widest-supported dialect rather than a silent drop.
        NotifierBackend::Osc => Route::Osc(osc.unwrap_or(OscNotifier::Osc777)),
        NotifierBackend::Auto => match (remote, osc, subprocess_available) {
            (true, Some(n), _) => Route::Osc(n),
            (_, _, true) => Route::Subprocess,
            (_, Some(n), false) => Route::Osc(n),
            (_, None, false) => Route::Subprocess,
        },
    }
}

/// Whether this process runs at the far end of an SSH connection —
/// where a spawned notifier helper would banner the remote host's
/// (headless) desktop instead of the user's.
fn remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// Whether a subprocess notifier can fire on this platform. macOS
/// always can (`osascript` ships with the OS); Linux needs
/// `notify-send` on PATH; Windows has no helper yet.
fn subprocess_notifier_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        notify_send_path().is_some()
    }
    #[cfg(windows)]
    {
        false
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn notify_send_path() -> Option<&'static std::path::Path> {
    use std::sync::OnceLock;
    static NOTIFY_SEND: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    NOTIFY_SEND
        .get_or_init(|| which::which("notify-send").ok())
        .as_deref()
}

/// Fire a desktop notification with `title` + `body`. Best-effort —
/// returns immediately whether or not the OS surface is available.
/// Every attempt logs its chosen backend at debug level so "where
/// did my banner go" is answerable from `/tmp/lazybox.log`.
///
/// Used to surface agent state changes that need the user's
/// attention even when lazybox isn't the focused app — e.g. Claude
/// going to `Asking` while the user is reading email.
///
/// Suppressed while lazybox's terminal is reported focused (see
/// [`crate::notify::terminal_is_focused`]) — a banner for what the
/// user is already looking at is pure noise.
///
/// Delivery is resolved by [`resolve_route`] from the backend armed
/// at startup ([`set_notifier_backend`], `attention.notifier` in
/// YAML). The OSC path never writes stdout here — the sequence is
/// queued for the render thread to emit between frames
/// ([`crate::notify::queue_osc_notification`]), so the escape bytes
/// can't interleave with a ratatui frame flush (issue #296).
pub fn notify_user(title: &str, body: &str) {
    let Some(backend) = notifier_backend() else {
        tracing::debug!(title, "notify_user: skipped — no backend armed");
        return;
    };
    // Don't self-spam: when lazybox's own terminal is reported focused
    // the user is already looking at it. Unknown focus (terminal
    // never reported it) falls through and still notifies.
    if crate::notify::terminal_is_focused() {
        tracing::debug!(title, "notify_user: suppressed — terminal reported focused");
        return;
    }
    let osc = crate::notify::detect_osc_notifier();
    let remote = remote_session();
    match resolve_route(backend, osc, remote, subprocess_notifier_available()) {
        Route::Osc(notifier) => {
            tracing::debug!(
                title,
                ?notifier,
                remote,
                "notify_user: OSC backend — queued for the render thread"
            );
            crate::notify::queue_osc_notification(notifier, title, body);
        }
        Route::Subprocess => {
            tracing::debug!(title, remote, "notify_user: subprocess backend");
            notify_subprocess(title, body);
        }
    }
}

/// Reap a spawned notifier helper on a detached thread and log its
/// exit status — the subprocess path's verifiability is the reason
/// `Auto` prefers it locally, so a helper that failed must leave a
/// trace in the log rather than vanish with the banner.
fn log_notifier_exit(helper: &'static str, spawned: std::io::Result<std::process::Child>) {
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || match child.wait() {
                Ok(status) if status.success() => {
                    tracing::debug!(helper, "notify_user: helper exited ok");
                }
                Ok(status) => {
                    tracing::warn!(helper, %status, "notify_user: helper failed — banner likely lost");
                }
                Err(e) => {
                    tracing::warn!(helper, error = %e, "notify_user: helper wait failed");
                }
            });
        }
        Err(e) => {
            tracing::warn!(helper, error = %e, "notify_user: helper failed to spawn");
        }
    }
}

/// Subprocess delivery path.
///
/// **macOS**: prefers `terminal-notifier` when it's on PATH — it
/// ships its own bundle, so the banner carries a real icon and
/// `-group` collapses repeats into one stack. When it's missing we
/// fall back to `osascript -e 'display notification …'`, which is
/// part of every macOS install and needs no `brew`. The tradeoff:
/// newer macOS attributes the osascript banner's click action to
/// Script Editor, so clicking it opens an empty AppleScript window.
/// A banner that appears (and is read at a glance) beats no banner at
/// all, so we accept the worse click target rather than stay silent
/// on a stock Mac — but we log a one-time hint (grep
/// `/tmp/lazybox.log`) pointing at `terminal-notifier`, which fixes
/// the click target. (When lazybox ships as a `.app` with its own
/// bundle id this whole fallback goes away.)
///
/// **Linux**: `notify-send` (libnotify), present on every desktop we
/// realistically support. Skipped with a one-time log if it's
/// missing — install `libnotify` (e.g. `apt install libnotify-bin`).
///
/// **Windows**: stub (TODO: PowerShell `New-BurntToastNotification`).
fn notify_subprocess(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        // Cache the `terminal-notifier` lookup so we don't spawn
        // `which` on every notification. `OnceLock` is `Sync`, safe
        // to share across the threads that fire notifications.
        use std::sync::OnceLock;
        static TERMINAL_NOTIFIER: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
        let tn = TERMINAL_NOTIFIER.get_or_init(|| which::which("terminal-notifier").ok());

        let stdio = || {
            (
                std::process::Stdio::null(),
                std::process::Stdio::null(),
                std::process::Stdio::null(),
            )
        };
        if let Some(tn_path) = tn {
            // `-sender` is intentionally omitted — without a real
            // lazybox.app bundle id, spoofing one would surface the
            // wrong app's icon.
            let (i, o, e) = stdio();
            let spawned = std::process::Command::new(tn_path)
                .arg("-title")
                .arg(title)
                .arg("-message")
                .arg(body)
                .arg("-group")
                .arg("com.lazybox.agent")
                .stdin(i)
                .stdout(o)
                .stderr(e)
                .spawn();
            log_notifier_exit("terminal-notifier", spawned);
        } else {
            // Zero-dependency fallback, but newer macOS attributes its
            // banner to Script Editor (clicking opens an empty
            // AppleScript window). Log a one-time hint so users who
            // wonder why their notifications "look broken" can grep
            // /tmp/lazybox.log and find the fix.
            static WARNED: OnceLock<()> = OnceLock::new();
            WARNED.get_or_init(|| {
                tracing::info!("{}", TERMINAL_NOTIFIER_HINT);
            });
            let script = osascript_notification_script(title, body);
            let (i, o, e) = stdio();
            let spawned = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .stdin(i)
                .stdout(o)
                .stderr(e)
                .spawn();
            log_notifier_exit("osascript", spawned);
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let Some(ns_path) = notify_send_path() else {
            // No notify-send on PATH → no notification. A one-time
            // tracing line so users can grep /tmp/lazybox.log and find
            // the install hint when they wonder where their
            // notifications went.
            use std::sync::OnceLock;
            static WARNED: OnceLock<()> = OnceLock::new();
            WARNED.get_or_init(|| {
                tracing::info!(
                    "notify_user: notify-send not found on PATH; desktop \
                     notifications disabled. Install libnotify \
                     (e.g. `apt install libnotify-bin`) to enable."
                );
            });
            let _ = (title, body);
            return;
        };
        let spawned = std::process::Command::new(ns_path)
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        log_notifier_exit("notify-send", spawned);
    }
    #[cfg(windows)]
    {
        let _ = (title, body);
    }
}

/// One-time hint logged when the osascript fallback fires because
/// `terminal-notifier` isn't installed. The osascript banner is
/// attributed to Script Editor on current macOS; `terminal-notifier`
/// ships its own bundle and restores a real click target.
#[cfg(target_os = "macos")]
const TERMINAL_NOTIFIER_HINT: &str = "notify_user: terminal-notifier not found on PATH; using the osascript fallback, \
     whose banner macOS attributes to Script Editor (clicking it opens an empty \
     AppleScript window). Install terminal-notifier (e.g. `brew install \
     terminal-notifier`) for a Notification Center banner with a proper click target.";

/// Build the `osascript -e` argument that fires a `display
/// notification` banner. The strings are interpolated into an
/// AppleScript literal, so they go through [`applescript_escape`].
#[cfg(target_os = "macos")]
fn osascript_notification_script(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title),
    )
}

/// Escape a string for embedding inside an AppleScript double-quoted
/// literal: backslash first (so we don't double-escape the escapes we
/// add next), then the double-quote.
#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Async wait for a graceful-shutdown signal — SIGTERM or Ctrl-C on
/// unix, Ctrl-Break on Windows. Resolves once. Used by
/// `lazybox server start`'s outer task to trigger a clean stop.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = ctrl_c => {},
        }
    }
    #[cfg(windows)]
    {
        // TODO(windows): tokio::signal::windows::{ctrl_c, ctrl_break}
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::notify::OscNotifier;

    #[test]
    fn auto_prefers_subprocess_locally_and_osc_over_ssh() {
        // Local with a helper available: subprocess, even in an
        // OSC-capable terminal — it's verifiable and immune to OSC
        // quirks (issue #296).
        assert_eq!(
            resolve_route(NotifierBackend::Auto, Some(OscNotifier::Osc777), false, true),
            Route::Subprocess
        );
        // Over SSH the helper would banner the remote host — OSC is
        // the only surface that reaches the local machine.
        assert_eq!(
            resolve_route(NotifierBackend::Auto, Some(OscNotifier::Osc9), true, true),
            Route::Osc(OscNotifier::Osc9)
        );
        // Local without a helper (Linux missing notify-send) still
        // uses an OSC-capable terminal rather than dropping the banner.
        assert_eq!(
            resolve_route(NotifierBackend::Auto, Some(OscNotifier::Osc777), false, false),
            Route::Osc(OscNotifier::Osc777)
        );
        // Nothing available: best-effort subprocess (no-op stub or a
        // remote-host banner) rather than silence.
        assert_eq!(
            resolve_route(NotifierBackend::Auto, None, true, false),
            Route::Subprocess
        );
    }

    #[test]
    fn explicit_backends_are_not_second_guessed() {
        assert_eq!(
            resolve_route(NotifierBackend::Subprocess, Some(OscNotifier::Osc777), true, false),
            Route::Subprocess
        );
        assert_eq!(
            resolve_route(NotifierBackend::Osc, Some(OscNotifier::Osc9), false, true),
            Route::Osc(OscNotifier::Osc9)
        );
        // `osc` with an unrecognized terminal (TERM_PROGRAM lost over
        // SSH) falls back to the widest dialect instead of dropping.
        assert_eq!(
            resolve_route(NotifierBackend::Osc, None, false, true),
            Route::Osc(OscNotifier::Osc777)
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn applescript_escape_neutralizes_quotes_and_backslashes() {
        assert_eq!(applescript_escape("plain"), "plain");
        // Backslash is escaped first so the quote-escape's own
        // backslash isn't doubled.
        assert_eq!(applescript_escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(applescript_escape(r#"a\b"#), r#"a\\b"#);
        assert_eq!(applescript_escape(r#"\""#), r#"\\\""#);
    }

    #[test]
    fn osascript_script_embeds_escaped_title_and_body() {
        let script = osascript_notification_script("Needs input", "PR #61");
        assert_eq!(
            script,
            r#"display notification "PR #61" with title "Needs input""#
        );
        // A title/body carrying a quote can't break out of the literal.
        let script = osascript_notification_script(r#"a"b"#, r#"c"d"#);
        assert_eq!(script, r#"display notification "c\"d" with title "a\"b""#);
    }

    #[test]
    fn terminal_notifier_hint_points_at_the_fix() {
        // The hint must name the helper to install and the symptom it
        // cures, so a user grepping the log gets an actionable fix.
        assert!(TERMINAL_NOTIFIER_HINT.contains("terminal-notifier"));
        assert!(TERMINAL_NOTIFIER_HINT.contains("brew install terminal-notifier"));
        assert!(TERMINAL_NOTIFIER_HINT.contains("Script Editor"));
    }
}
