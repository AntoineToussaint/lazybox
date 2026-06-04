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

/// Fire a desktop notification with `title` + `body`. Best-effort —
/// returns immediately whether or not the OS surface is available
/// (no PII in error logs; we don't want a missing dependency to
/// generate noise on every notification).
///
/// Used to surface agent state changes that need the user's
/// attention even when lazybox isn't the focused app — e.g. Claude
/// going to `Asking` while the user is reading email.
///
/// Suppressed while lazybox's terminal is reported focused (see
/// [`crate::notify::terminal_is_focused`]) — a banner for what the
/// user is already looking at is pure noise.
///
/// **OSC-capable terminals** (Ghostty / iTerm2 / Kitty / WezTerm):
/// the banner is emitted as an OSC escape sequence written to the
/// controlling terminal ([`crate::notify`]). This is preferred over
/// the subprocess paths below because it reaches the *local* machine
/// even when lazybox runs over SSH, and needs no helper binary.
///
/// The subprocess paths below are the fallback for terminals without
/// OSC notification support (Terminal.app, plain SSH, …):
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
/// on a stock Mac. (When lazybox ships as a `.app` with its own bundle
/// id this whole fallback goes away.)
///
/// **Linux**: `notify-send` (libnotify), present on every desktop we
/// realistically support. Skipped with a one-time log if it's
/// missing — install `libnotify` (e.g. `apt install libnotify-bin`).
///
/// **Windows**: stub (TODO: PowerShell `New-BurntToastNotification`).
pub fn notify_user(title: &str, body: &str) {
    // Don't self-spam: when lazybox's own terminal is reported focused
    // the user is already looking at it. Unknown focus (terminal
    // never reported it) falls through and still notifies.
    if crate::notify::terminal_is_focused() {
        return;
    }
    // Prefer the terminal's own OSC notification surface — it reaches
    // the local machine even when lazybox runs over SSH (the subprocess
    // fallbacks would fire on the remote host, where no one is
    // looking) and needs no helper binary.
    if let Some(notifier) = crate::notify::detect_osc_notifier() {
        crate::notify::emit_osc_notification(notifier, title, body);
        return;
    }
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
            let _ = std::process::Command::new(tn_path)
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
        } else {
            // Zero-dependency fallback. The strings are interpolated
            // into an AppleScript literal, so escape `\` and `"` to
            // keep a title/body with quotes from breaking the script.
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                applescript_escape(body),
                applescript_escape(title),
            );
            let (i, o, e) = stdio();
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .stdin(i)
                .stdout(o)
                .stderr(e)
                .spawn();
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::sync::OnceLock;
        static NOTIFY_SEND: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
        let ns = NOTIFY_SEND.get_or_init(|| which::which("notify-send").ok());

        let Some(ns_path) = ns else {
            // No notify-send on PATH → no notification. A one-time
            // tracing line so users can grep /tmp/lazybox.log and find
            // the install hint when they wonder where their
            // notifications went.
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
        let _ = std::process::Command::new(ns_path)
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(windows)]
    {
        let _ = (title, body);
    }
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
