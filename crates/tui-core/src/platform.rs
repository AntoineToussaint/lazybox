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

#[cfg(target_os = "macos")]
struct NotificationClickContext {
    executable: std::path::PathBuf,
    socket_path: std::path::PathBuf,
    terminal_bundle_id: String,
    terminal_session: Option<TerminalSession>,
}

#[cfg(target_os = "macos")]
enum TerminalSession {
    Tty(String),
    WezTermPane(String),
}

#[cfg(target_os = "macos")]
static NOTIFICATION_CLICK_CONTEXT: std::sync::OnceLock<Option<NotificationClickContext>> =
    std::sync::OnceLock::new();

/// Configure the local target a clickable macOS notification should
/// reach. The socket differs for `--connect <path>` clients, so the
/// boot crate supplies the path used by this TUI instead of assuming
/// the default daemon location.
#[cfg(target_os = "macos")]
pub fn set_notification_click_context(
    socket_path: Option<std::path::PathBuf>,
    configured_bundle_id: Option<String>,
) {
    let terminal_bundle_id = detect_terminal_bundle_id(
        configured_bundle_id.as_deref(),
        std::env::var("__CFBundleIdentifier").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    );
    let terminal_session = terminal_bundle_id
        .as_deref()
        .and_then(detect_terminal_session);
    let context = socket_path
        .zip(std::env::current_exe().ok())
        .zip(terminal_bundle_id)
        .map(
            |((socket_path, executable), terminal_bundle_id)| NotificationClickContext {
                executable,
                socket_path,
                terminal_bundle_id,
                terminal_session,
            },
        );
    let _ = NOTIFICATION_CLICK_CONTEXT.set(context);
}

#[cfg(not(target_os = "macos"))]
pub fn set_notification_click_context(
    socket_path: Option<std::path::PathBuf>,
    configured_bundle_id: Option<String>,
) {
    let _ = (socket_path, configured_bundle_id);
}

/// Resolve the app to bring forward for a notification click. A
/// configured value wins; macOS's inherited bundle identifier is more
/// precise than terminal-name inference and also covers integrated
/// terminals such as VS Code.
pub fn detect_terminal_bundle_id(
    configured: Option<&str>,
    inherited: Option<&str>,
    term_program: Option<&str>,
) -> Option<String> {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| inherited.map(str::trim).filter(|value| !value.is_empty()))
        .map(str::to_string)
        .or_else(|| {
            let bundle_id = match term_program? {
                "Apple_Terminal" => "com.apple.Terminal",
                "iTerm.app" => "com.googlecode.iterm2",
                "ghostty" => "com.mitchellh.ghostty",
                "WezTerm" => "com.github.wez.wezterm",
                _ => return None,
            };
            Some(bundle_id.to_string())
        })
}

#[cfg(target_os = "macos")]
fn detect_terminal_session(bundle_id: &str) -> Option<TerminalSession> {
    if bundle_id == "com.github.wez.wezterm" {
        return std::env::var("WEZTERM_PANE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(TerminalSession::WezTermPane);
    }
    if !matches!(bundle_id, "com.apple.Terminal" | "com.googlecode.iterm2") {
        return None;
    }

    let tty = if std::env::var_os("TMUX").is_some() {
        command_stdout("tmux", &["display-message", "-p", "#{client_tty}"])
    } else {
        None
    }
    .or_else(|| command_stdout("tty", &[]))?;
    Some(TerminalSession::Tty(tty))
}

#[cfg(target_os = "macos")]
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

/// What the subprocess path would actually run, graded by how much
/// its exit status can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubprocessNotifier {
    /// A dedicated helper whose success is meaningful:
    /// `terminal-notifier` (macOS) or `notify-send` (Linux).
    Dedicated,
    /// The `osascript` fallback: ships with every macOS install, but
    /// `display notification` exits 0 even when the banner never
    /// appears (Script Editor's notification permission denied, Focus
    /// mode) — so "verifiable" doesn't hold for it.
    // Each variant below is constructed only under its platform's cfg
    // arm in `subprocess_notifier`; the enum itself is deliberately
    // platform-complete so `resolve_route` stays pure and testable.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Osascript,
    /// No helper at all (Linux without `notify-send`, Windows).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unavailable,
}

/// Pick the delivery path. Pure so the policy is unit-testable.
///
/// `Auto` prefers a *dedicated* subprocess helper locally: it's
/// verifiable (exit status lands in the log) and immune to
/// terminal/OSC quirks — an unhandled or corrupted escape sequence
/// means a silently lost banner at best, literal junk on screen at
/// worst. But that reasoning doesn't extend to `osascript`, which
/// reports success even when macOS suppresses its banner — a
/// recognized OSC-capable terminal's own notification surface beats
/// it, so osascript stays the last resort. Over SSH the helpers
/// would fire on the remote host where no one is looking, so OSC
/// (which the local emulator renders) wins there.
fn resolve_route(
    backend: NotifierBackend,
    osc: Option<crate::notify::OscNotifier>,
    remote: bool,
    subprocess: SubprocessNotifier,
) -> Route {
    use crate::notify::OscNotifier;
    match backend {
        NotifierBackend::Subprocess => Route::Subprocess,
        // Explicit `osc` trusts the user over detection: `$TERM_PROGRAM`
        // often doesn't survive an SSH hop, so an unrecognized terminal
        // gets the widest-supported dialect rather than a silent drop.
        NotifierBackend::Osc => Route::Osc(osc.unwrap_or(OscNotifier::Osc777)),
        NotifierBackend::Auto => match (remote, osc, subprocess) {
            (true, Some(n), _) => Route::Osc(n),
            (_, _, SubprocessNotifier::Dedicated) => Route::Subprocess,
            (_, Some(n), _) => Route::Osc(n),
            (_, None, _) => Route::Subprocess,
        },
    }
}

/// Whether this process runs at the far end of an SSH connection —
/// where a spawned notifier helper would banner the remote host's
/// (headless) desktop instead of the user's.
fn remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// Grade the subprocess notifier this platform would run — see
/// [`SubprocessNotifier`].
fn subprocess_notifier() -> SubprocessNotifier {
    #[cfg(target_os = "macos")]
    {
        if terminal_notifier_path().is_some() {
            SubprocessNotifier::Dedicated
        } else {
            SubprocessNotifier::Osascript
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if notify_send_path().is_some() {
            SubprocessNotifier::Dedicated
        } else {
            SubprocessNotifier::Unavailable
        }
    }
    #[cfg(windows)]
    {
        SubprocessNotifier::Unavailable
    }
}

/// Cached `terminal-notifier` lookup so we don't spawn `which` on
/// every notification. `OnceLock` is `Sync`, safe to share across the
/// threads that fire notifications.
#[cfg(target_os = "macos")]
fn terminal_notifier_path() -> Option<&'static std::path::Path> {
    use std::sync::OnceLock;
    static TERMINAL_NOTIFIER: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    TERMINAL_NOTIFIER
        .get_or_init(|| which::which("terminal-notifier").ok())
        .as_deref()
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
/// Delivery is resolved by `resolve_route` from the backend armed
/// at startup ([`set_notifier_backend`], `attention.notifier` in
/// YAML). The OSC path never writes stdout here — the sequence is
/// queued for the render thread to emit between frames
/// ([`crate::notify::queue_osc_notification`]), so the escape bytes
/// can't interleave with a ratatui frame flush (issue #296).
pub fn notify_user(title: &str, body: &str, workspace_key: &lazybox_core::SessionKey) {
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
    match resolve_route(backend, osc, remote, subprocess_notifier()) {
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
            notify_subprocess(title, body, workspace_key);
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
fn notify_subprocess(title: &str, body: &str, workspace_key: &lazybox_core::SessionKey) {
    #[cfg(target_os = "macos")]
    {
        use std::sync::OnceLock;
        let tn = terminal_notifier_path();

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
            let mut command = std::process::Command::new(tn_path);
            command
                .arg("-title")
                .arg(title)
                .arg("-message")
                .arg(body)
                .arg("-group")
                .arg("com.lazybox.agent");
            if let Some(click_command) = notification_click_command(workspace_key) {
                command.arg("-execute").arg(click_command);
            }
            let spawned = command.stdin(i).stdout(o).stderr(e).spawn();
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
        let _ = workspace_key;
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
        let _ = (title, body, workspace_key);
    }
}

#[cfg(target_os = "macos")]
fn notification_click_command(workspace_key: &lazybox_core::SessionKey) -> Option<String> {
    let context = NOTIFICATION_CLICK_CONTEXT.get()?.as_ref()?;
    let executable = context.executable.to_str()?;
    let socket_path = context.socket_path.to_str()?;
    Some(build_notification_click_command(
        executable,
        socket_path,
        &context.terminal_bundle_id,
        context.terminal_session.as_ref(),
        workspace_key,
    ))
}

#[cfg(target_os = "macos")]
fn build_notification_click_command(
    executable: &str,
    socket_path: &str,
    terminal_bundle_id: &str,
    terminal_session: Option<&TerminalSession>,
    workspace_key: &lazybox_core::SessionKey,
) -> String {
    let mut command = format!(
        "{} notification-click --workspace {} --socket {} --terminal-bundle-id {}",
        shell_quote(executable),
        shell_quote(workspace_key.as_str()),
        shell_quote(socket_path),
        shell_quote(terminal_bundle_id),
    );
    match terminal_session {
        Some(TerminalSession::Tty(tty)) => {
            command.push_str(" --terminal-tty ");
            command.push_str(&shell_quote(tty));
        }
        Some(TerminalSession::WezTermPane(pane_id)) => {
            command.push_str(" --wezterm-pane-id ");
            command.push_str(&shell_quote(pane_id));
        }
        None => {}
    }
    command
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
            resolve_route(
                NotifierBackend::Auto,
                Some(OscNotifier::Osc777),
                false,
                SubprocessNotifier::Dedicated
            ),
            Route::Subprocess
        );
        // Over SSH the helper would banner the remote host — OSC is
        // the only surface that reaches the local machine.
        assert_eq!(
            resolve_route(
                NotifierBackend::Auto,
                Some(OscNotifier::Osc9),
                true,
                SubprocessNotifier::Dedicated
            ),
            Route::Osc(OscNotifier::Osc9)
        );
        // Local with only osascript (no terminal-notifier): a
        // recognized terminal's own banner wins — osascript reports
        // success even when macOS suppresses it, so it's the last
        // resort, not the preferred "verifiable" path.
        assert_eq!(
            resolve_route(
                NotifierBackend::Auto,
                Some(OscNotifier::Osc777),
                false,
                SubprocessNotifier::Osascript
            ),
            Route::Osc(OscNotifier::Osc777)
        );
        // Local without any helper (Linux missing notify-send) still
        // uses an OSC-capable terminal rather than dropping the banner.
        assert_eq!(
            resolve_route(
                NotifierBackend::Auto,
                Some(OscNotifier::Osc777),
                false,
                SubprocessNotifier::Unavailable
            ),
            Route::Osc(OscNotifier::Osc777)
        );
        // Unrecognized terminal and only osascript: best-effort
        // osascript beats silence.
        assert_eq!(
            resolve_route(
                NotifierBackend::Auto,
                None,
                false,
                SubprocessNotifier::Osascript
            ),
            Route::Subprocess
        );
        // Nothing available: best-effort subprocess (no-op stub or a
        // remote-host banner) rather than silence.
        assert_eq!(
            resolve_route(
                NotifierBackend::Auto,
                None,
                true,
                SubprocessNotifier::Unavailable
            ),
            Route::Subprocess
        );
    }

    #[test]
    fn explicit_backends_are_not_second_guessed() {
        assert_eq!(
            resolve_route(
                NotifierBackend::Subprocess,
                Some(OscNotifier::Osc777),
                true,
                SubprocessNotifier::Unavailable
            ),
            Route::Subprocess
        );
        assert_eq!(
            resolve_route(
                NotifierBackend::Osc,
                Some(OscNotifier::Osc9),
                false,
                SubprocessNotifier::Dedicated
            ),
            Route::Osc(OscNotifier::Osc9)
        );
        // `osc` with an unrecognized terminal (TERM_PROGRAM lost over
        // SSH) falls back to the widest dialect instead of dropping.
        assert_eq!(
            resolve_route(
                NotifierBackend::Osc,
                None,
                false,
                SubprocessNotifier::Dedicated
            ),
            Route::Osc(OscNotifier::Osc777)
        );
    }

    #[test]
    fn terminal_bundle_detection_prefers_exact_sources_then_known_terminals() {
        assert_eq!(
            detect_terminal_bundle_id(
                Some("  com.example.Override  "),
                Some("com.example.Inherited"),
                Some("ghostty")
            )
            .as_deref(),
            Some("com.example.Override")
        );
        assert_eq!(
            detect_terminal_bundle_id(None, Some("com.microsoft.VSCode"), Some("unknown"))
                .as_deref(),
            Some("com.microsoft.VSCode")
        );
        assert_eq!(
            detect_terminal_bundle_id(None, None, Some("Apple_Terminal")).as_deref(),
            Some("com.apple.Terminal")
        );
        assert_eq!(
            detect_terminal_bundle_id(None, None, Some("iTerm.app")).as_deref(),
            Some("com.googlecode.iterm2")
        );
        assert_eq!(
            detect_terminal_bundle_id(None, None, Some("ghostty")).as_deref(),
            Some("com.mitchellh.ghostty")
        );
        assert_eq!(
            detect_terminal_bundle_id(None, None, Some("WezTerm")).as_deref(),
            Some("com.github.wez.wezterm")
        );
        assert_eq!(detect_terminal_bundle_id(None, None, Some("unknown")), None);
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

    #[test]
    fn notification_click_command_quotes_every_dynamic_argument() {
        let command = build_notification_click_command(
            "/Applications/Lazy Box/lazybox",
            "/tmp/lazy'box/daemon.sock",
            "com.example.Terminal",
            Some(&TerminalSession::Tty("/dev/ttys674".into())),
            &lazybox_core::SessionKey::new("github:o/repo#674; touch /tmp/no"),
        );
        assert_eq!(
            command,
            "'/Applications/Lazy Box/lazybox' notification-click --workspace \
             'github:o/repo#674; touch /tmp/no' --socket \
             '/tmp/lazy'\"'\"'box/daemon.sock' --terminal-bundle-id \
             'com.example.Terminal' --terminal-tty '/dev/ttys674'"
        );
    }

    #[test]
    fn notification_click_context_uses_the_client_socket_and_bundle_override() {
        set_notification_click_context(
            Some(std::path::PathBuf::from("/tmp/lazybox-remote.sock")),
            Some("com.example.Terminal".into()),
        );
        let command = notification_click_command(&lazybox_core::SessionKey::new("github:o/r#674"))
            .expect("click command");
        assert!(command.contains(
            " notification-click --workspace 'github:o/r#674' --socket \
             '/tmp/lazybox-remote.sock' --terminal-bundle-id 'com.example.Terminal'"
        ));
    }
}
