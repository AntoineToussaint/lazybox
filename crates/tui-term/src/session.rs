use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

#[derive(Debug, thiserror::Error)]
pub enum TermError {
    #[error("failed to open PTY: {0}")]
    PtyOpen(Box<dyn std::error::Error + Send + Sync>),
    #[error("failed to spawn child: {0}")]
    Spawn(Box<dyn std::error::Error + Send + Sync>),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal error: {0}")]
    Terminal(String),
}

/// Manages a PTY child process + libghostty-vt terminal state.
///
/// PTY reader runs on a background thread, sends bytes via channel.
/// Call `process_pending()` from the main thread to feed bytes into
/// the terminal (libghostty is !Send + !Sync).
///
/// This type is intentionally `!Send + !Sync`: libghostty holds raw pointers
/// that are unsound to share across threads. The `_not_send` marker makes
/// that constraint explicit in the type, so cross-thread use is caught at
/// compile time with a clear error instead of an auto-trait failure.
pub struct TermSession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    _reader_thread: std::thread::JoinHandle<()>,
    size: PtySize,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pty_rx: mpsc::Receiver<Vec<u8>>,
    terminal: libghostty_vt::Terminal<'static, 'static>,
    render_state: libghostty_vt::RenderState<'static>,
    row_iter: libghostty_vt::render::RowIterator<'static>,
    cell_iter: libghostty_vt::render::CellIterator<'static>,
    /// Per-session render cache — see `GhosttyTerminal` for the
    /// dirty-tracking flow that uses it.
    shadow: Option<ratatui::buffer::Buffer>,
    /// When the PTY last produced output.
    last_output_at: Instant,
    /// Rolling buffer of recent PTY output (last ~4KB) for callers to inspect.
    recent_output: Vec<u8>,
    /// True when the viewport has been scrolled up into scrollback history.
    /// Used to display the `[SCROLLBACK]` indicator.
    scrolled_back: bool,
    /// Explicit `!Send + !Sync` marker — see struct doc.
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl TermSession {
    pub fn spawn(
        cmd: &[&str],
        size: PtySize,
        cwd: Option<&Path>,
        env: Vec<(String, String)>,
    ) -> Result<Self, TermError> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(size)
            .map_err(|e| TermError::PtyOpen(e.into()))?;

        let program = cmd
            .first()
            .ok_or_else(|| TermError::Spawn("empty command".to_string().into()))?;
        let mut command = CommandBuilder::new(program);
        for arg in &cmd[1..] {
            command.arg(arg);
        }
        if let Some(dir) = cwd {
            command.cwd(dir);
        }
        for (k, v) in env {
            command.env(k, v);
        }
        command.env("TERM", "xterm-256color");

        let _child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| TermError::Spawn(e.into()))?;
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TermError::PtyOpen(e.into()))?;

        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>();

        let reader_finished = finished.clone();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TermError::PtyOpen(e.into()))?;

        let reader_thread = std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            tracing::debug!("PTY reader: EOF");
                            break;
                        }
                        Ok(n) => {
                            if pty_tx.send(buf[..n].to_vec()).is_err() {
                                tracing::debug!("PTY reader: receiver dropped");
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            tracing::warn!("PTY reader error: {e}");
                            break;
                        }
                    }
                }
                // Release so `is_finished()` using Acquire sees everything the
                // reader did before setting the flag.
                reader_finished.store(true, std::sync::atomic::Ordering::Release);
            })
            .map_err(|e| TermError::Spawn(Box::new(e)))?;

        let terminal = libghostty_vt::Terminal::new(libghostty_vt::TerminalOptions {
            cols: size.cols,
            rows: size.rows,
            max_scrollback: 10_000,
        })
        .map_err(|e| TermError::Terminal(e.to_string()))?;

        let render_state =
            libghostty_vt::RenderState::new().map_err(|e| TermError::Terminal(e.to_string()))?;
        let row_iter = libghostty_vt::render::RowIterator::new()
            .map_err(|e| TermError::Terminal(e.to_string()))?;
        let cell_iter = libghostty_vt::render::CellIterator::new()
            .map_err(|e| TermError::Terminal(e.to_string()))?;

        Ok(Self {
            writer,
            master: pair.master,
            _reader_thread: reader_thread,
            size,
            finished,
            pty_rx,
            terminal,
            render_state,
            row_iter,
            cell_iter,
            shadow: None,
            last_output_at: Instant::now(),
            recent_output: Vec::with_capacity(4096),
            scrolled_back: false,
            _not_send: std::marker::PhantomData,
        })
    }

    /// Process pending PTY output — call from main thread every tick.
    /// Returns true if new output was received.
    pub fn process_pending(&mut self) -> bool {
        let mut had_output = false;
        while let Ok(chunk) = self.pty_rx.try_recv() {
            self.terminal.vt_write(&chunk);
            // Buffer recent output for callers to inspect.
            self.recent_output.extend_from_slice(&chunk);
            if self.recent_output.len() > 4096 {
                let excess = self.recent_output.len() - 4096;
                self.recent_output.copy_within(excess.., 0);
                self.recent_output.truncate(4096);
            }
            had_output = true;
        }
        if had_output {
            self.last_output_at = Instant::now();
        }
        had_output
    }

    /// When the PTY last produced output.
    pub fn last_output_at(&self) -> Instant {
        self.last_output_at
    }

    /// Recent PTY output bytes (last ~4KB) for pattern detection by callers.
    pub fn recent_output(&self) -> &[u8] {
        &self.recent_output
    }

    /// Send raw bytes to the PTY.
    pub fn write(&mut self, data: &[u8]) -> Result<(), TermError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY and terminal.
    pub fn resize(&mut self, size: PtySize) -> Result<(), TermError> {
        if size.rows != self.size.rows || size.cols != self.size.cols {
            self.master
                .resize(size)
                .map_err(|e| TermError::PtyOpen(e.into()))?;
            let _ = self.terminal.resize(size.cols, size.rows, 0, 0);
            self.size = size;
        }
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        // Acquire pairs with the reader thread's Release on exit — guarantees
        // we see everything the reader did before it set the flag.
        self.finished.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn size(&self) -> PtySize {
        self.size
    }

    /// Access terminal + render state for widget rendering.
    pub fn render_data(
        &mut self,
    ) -> (
        &mut libghostty_vt::Terminal<'static, 'static>,
        &mut libghostty_vt::RenderState<'static>,
        &mut libghostty_vt::render::RowIterator<'static>,
        &mut libghostty_vt::render::CellIterator<'static>,
        &mut Option<ratatui::buffer::Buffer>,
    ) {
        (
            &mut self.terminal,
            &mut self.render_state,
            &mut self.row_iter,
            &mut self.cell_iter,
            &mut self.shadow,
        )
    }

    /// Scroll the viewport up by N lines (into the scrollback history).
    pub fn scroll_up(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.terminal
            .scroll_viewport(libghostty_vt::terminal::ScrollViewport::Delta(
                -(lines as isize),
            ));
        self.scrolled_back = true;
    }

    /// Scroll the viewport down by N lines (back toward the live area).
    pub fn scroll_down(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.terminal
            .scroll_viewport(libghostty_vt::terminal::ScrollViewport::Delta(
                lines as isize,
            ));
        // If we scrolled all the way back to the bottom, clear the flag.
        if let Ok(rows) = self.terminal.scrollback_rows()
            && rows == 0
        {
            self.scrolled_back = false;
        }
    }

    /// Jump back to the live (bottom) area.
    pub fn scroll_reset(&mut self) {
        self.terminal
            .scroll_viewport(libghostty_vt::terminal::ScrollViewport::Bottom);
        self.scrolled_back = false;
    }

    pub fn is_scrolled(&self) -> bool {
        self.scrolled_back
    }

    /// True when the inner app has enabled any mouse tracking mode.
    /// When this is on, we should forward scroll wheel events as raw
    /// SGR mouse escape sequences so tmux/vim/less/Claude Code can
    /// drive their own scroll behavior. When off, forwarding mouse as
    /// *anything* (including arrow keys — don't!) corrupts the input
    /// stream, so we fall back to libghostty's scrollback instead.
    pub fn is_mouse_tracking(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    /// True when the PTY child is using the alternate screen buffer
    /// (tmux, vim, less, etc.). Libghostty's scrollback is empty in this
    /// mode; the caller needs a different strategy — either forward
    /// mouse as SGR events (if mouse tracking is on) or do nothing.
    pub fn in_alternate_screen(&self) -> bool {
        self.terminal
            .mode(libghostty_vt::terminal::Mode::ALT_SCREEN)
            .unwrap_or(false)
            || self
                .terminal
                .mode(libghostty_vt::terminal::Mode::ALT_SCREEN_SAVE)
                .unwrap_or(false)
            || self
                .terminal
                .mode(libghostty_vt::terminal::Mode::ALT_SCREEN_LEGACY)
                .unwrap_or(false)
    }
}
