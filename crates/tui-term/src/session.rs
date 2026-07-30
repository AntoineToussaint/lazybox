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

/// Upper bound on the `recent_output` inspection buffer. Callers scan
/// this tail for agent-state / prompt markers, so it must stay large
/// enough to hold a marker whole.
const RECENT_OUTPUT_CAP: usize = 4096;

/// Bound `buf` to roughly the last `cap` bytes, trimming on a newline
/// boundary so the retained head starts on a clean line.
///
/// A raw byte cap can bisect a marker that straddles the eviction
/// boundary: its head falls in the dropped prefix while its tail stays
/// in the buffer in a form no longer matchable, so a `recent_output`
/// pattern scan silently misses the state transition. Dropping up to
/// and including the first newline at/after the cut keeps every
/// (line-anchored) marker either fully retained or fully gone, never
/// split. If the retained tail carries no newline we keep the raw cut so
/// the buffer stays bounded. Likewise, if that first newline is the
/// buffer's final byte the line-aligned cut would empty the tail
/// entirely — discarding a whole intact line the raw cut would have kept
/// — so we fall back to the raw cut there too. Mirrors
/// `ReplayRing::replay_snapshot_into` / `read_scrollback_tail` in
/// `crates/server/src/pty.rs`.
fn trim_recent_output(buf: &mut Vec<u8>, cap: usize) {
    if buf.len() <= cap {
        return;
    }
    let excess = buf.len() - cap;
    // Advance past the first newline at/after the raw cut, so the head
    // is line-aligned; fall back to the raw cut when the tail has none,
    // or when advancing would empty the buffer (newline at the very end).
    let cut = buf[excess..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| excess + rel + 1)
        .filter(|&c| c < buf.len())
        .unwrap_or(excess);
    buf.copy_within(cut.., 0);
    buf.truncate(buf.len() - cut);
}

struct TermState {
    pty_rx: mpsc::Receiver<Vec<u8>>,
    terminal: libghostty_vt::Terminal<'static, 'static>,
    recent_output: Vec<u8>,
    last_output_at: Instant,
}

impl TermState {
    fn new(
        pty_rx: mpsc::Receiver<Vec<u8>>,
        terminal: libghostty_vt::Terminal<'static, 'static>,
    ) -> Self {
        Self {
            pty_rx,
            terminal,
            recent_output: Vec::with_capacity(RECENT_OUTPUT_CAP),
            last_output_at: Instant::now(),
        }
    }

    fn process_pending(&mut self) -> bool {
        let mut had_output = false;
        while let Ok(chunk) = self.pty_rx.try_recv() {
            self.ingest(&chunk);
            had_output = true;
        }
        had_output
    }

    fn ingest(&mut self, chunk: &[u8]) {
        self.terminal.vt_write(chunk);
        self.recent_output.extend_from_slice(chunk);
        trim_recent_output(&mut self.recent_output, RECENT_OUTPUT_CAP);
        self.last_output_at = Instant::now();
    }
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
    state: TermState,
    render_state: libghostty_vt::RenderState<'static>,
    row_iter: libghostty_vt::render::RowIterator<'static>,
    cell_iter: libghostty_vt::render::CellIterator<'static>,
    /// Per-session render cache — see `GhosttyTerminal` for the
    /// dirty-tracking flow that uses it.
    shadow: Option<ratatui::buffer::Buffer>,
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
        // Modern TUIs gate full color on `COLORTERM=truecolor`; with
        // `TERM` alone many degrade to limited or no color (#421).
        command.env("COLORTERM", "truecolor");

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
            state: TermState::new(pty_rx, terminal),
            render_state,
            row_iter,
            cell_iter,
            shadow: None,
            _not_send: std::marker::PhantomData,
        })
    }

    /// Process pending PTY output — call from main thread every tick.
    /// Returns true if new output was received.
    pub fn process_pending(&mut self) -> bool {
        self.state.process_pending()
    }

    /// When the PTY last produced output.
    pub fn last_output_at(&self) -> Instant {
        self.state.last_output_at
    }

    /// Recent PTY output bytes (last ~4KB) for pattern detection by callers.
    pub fn recent_output(&self) -> &[u8] {
        &self.state.recent_output
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
            let _ = self.state.terminal.resize(size.cols, size.rows, 0, 0);
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
            &mut self.state.terminal,
            &mut self.render_state,
            &mut self.row_iter,
            &mut self.cell_iter,
            &mut self.shadow,
        )
    }

    /// True when the inner app has enabled any mouse tracking mode.
    /// Callers use this to forward clicks in the app's requested mouse
    /// protocol. Wheel events remain in the outer terminal's scrollback.
    pub fn is_mouse_tracking(&self) -> bool {
        self.state.terminal.is_mouse_tracking().unwrap_or(false)
    }

    /// True when the PTY child is using the alternate screen buffer
    /// (tmux, vim, less, etc.). Libghostty's scrollback is empty in this
    /// mode; the caller needs a different strategy — either forward
    /// mouse as SGR events (if mouse tracking is on) or do nothing.
    pub fn in_alternate_screen(&self) -> bool {
        self.state
            .terminal
            .mode(libghostty_vt::terminal::Mode::ALT_SCREEN)
            .unwrap_or(false)
            || self
                .state
                .terminal
                .mode(libghostty_vt::terminal::Mode::ALT_SCREEN_SAVE)
                .unwrap_or(false)
            || self
                .state
                .terminal
                .mode(libghostty_vt::terminal::Mode::ALT_SCREEN_LEGACY)
                .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{RECENT_OUTPUT_CAP, TermState, trim_recent_output};
    use libghostty_vt::{Terminal, TerminalOptions};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn term_state() -> (mpsc::Sender<Vec<u8>>, TermState) {
        let (tx, rx) = mpsc::channel();
        let terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .expect("create terminal");
        (tx, TermState::new(rx, terminal))
    }

    #[test]
    fn process_pending_ingests_queued_chunks_and_advances_timestamp() {
        let (tx, mut state) = term_state();
        let previous_output_at = Instant::now() - Duration::from_secs(1);
        state.last_output_at = previous_output_at;
        tx.send(b"hello ".to_vec()).expect("send first chunk");
        tx.send(b"world".to_vec()).expect("send second chunk");

        assert!(state.process_pending());
        assert_eq!(state.recent_output, b"hello world");
        assert_eq!(state.terminal.cursor_x().expect("read cursor"), 11);
        assert!(state.last_output_at > previous_output_at);

        let last_output_at = state.last_output_at;
        assert!(!state.process_pending());
        assert_eq!(state.last_output_at, last_output_at);
    }

    #[test]
    fn process_pending_trims_recent_output() {
        let (tx, mut state) = term_state();
        let output = vec![b'a'; RECENT_OUTPUT_CAP + 500];
        tx.send(output).expect("send output");

        assert!(state.process_pending());
        assert_eq!(state.recent_output, vec![b'a'; RECENT_OUTPUT_CAP]);
    }

    #[test]
    fn trim_keeps_buffer_bounded() {
        let mut buf = vec![b'a'; RECENT_OUTPUT_CAP + 500];
        // No newline anywhere: falls back to the raw cut, retaining
        // exactly the last `cap` bytes so the buffer stays bounded.
        trim_recent_output(&mut buf, RECENT_OUTPUT_CAP);
        assert_eq!(buf.len(), RECENT_OUTPUT_CAP);
    }

    #[test]
    fn trim_with_newline_at_the_very_end_keeps_the_final_line() {
        // Degenerate layout: the only newline in the eviction window is
        // the buffer's final byte. Line-aligning past it would empty the
        // tail, discarding a whole intact line the raw cut would keep —
        // so we fall back to the raw cut and stay non-empty + bounded.
        let cap = 16usize;
        const MARKER: &[u8] = b"<STATE>";
        // A single long line (no interior newline) carrying a marker near
        // its end, terminated by a newline exactly at the buffer's tail.
        let mut buf = b"padding padding <STATE>\n".to_vec();
        assert!(buf.len() > cap);

        trim_recent_output(&mut buf, cap);
        assert!(!buf.is_empty(), "tail is not emptied: {}", buf.len());
        assert!(buf.len() <= cap, "stays bounded: {}", buf.len());
        assert!(
            buf.windows(MARKER.len()).any(|w| w == MARKER),
            "the intact marker in the final line survives: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    #[test]
    fn trim_is_a_noop_below_cap() {
        let mut buf = b"short output\nno trim".to_vec();
        let before = buf.clone();
        trim_recent_output(&mut buf, RECENT_OUTPUT_CAP);
        assert_eq!(buf, before);
    }

    #[test]
    fn marker_spanning_the_raw_cut_stays_matchable_after_trim() {
        // Reproduces the boundary-bisection bug (#512). Layout: a
        // leading partial line holding a MARKER positioned so the raw
        // byte cutoff lands *inside* it, then a clean line carrying an
        // intact MARKER within the retained window.
        let cap = 32usize;
        const MARKER: &[u8] = b"<STATE>";
        //         ".xx<STATE>yy\nclean line <STATE> here\n"
        let mut buf = b".xx<STATE>yy\nclean line <STATE> here\n".to_vec();
        assert!(buf.len() > cap);

        // Old behavior (plain byte cap): the last `cap` bytes bisect the
        // first marker — the retained tail starts mid-marker, so the
        // *only* occurrence a scan can see is unusable.
        let raw_tail = &buf[buf.len() - cap..];
        assert!(
            raw_tail.starts_with(b"TATE>yy"),
            "fixture: raw cut bisects the first marker: {:?}",
            String::from_utf8_lossy(raw_tail)
        );

        trim_recent_output(&mut buf, cap);
        assert!(buf.len() <= cap, "stays bounded: {}", buf.len());
        // Line-aligned trim drops the whole partial leading line, so the
        // intact marker on the clean line survives and stays matchable.
        assert!(
            buf.starts_with(b"clean line"),
            "retained head is line-aligned: {:?}",
            String::from_utf8_lossy(&buf)
        );
        assert!(
            buf.windows(MARKER.len()).any(|w| w == MARKER),
            "intact marker still matchable: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }
}
