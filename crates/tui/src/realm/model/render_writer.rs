//! Off-thread terminal writer (#1090 / #1045).
//!
//! The run loop's `terminal.draw` does two things: build the frame (CPU,
//! ~1-2ms — proven cheap by the `render_walk` bench) and **flush the diff
//! to stdout** (I/O that blocks on the host terminal). Because the flush
//! runs on the input/render thread, a slow terminal — a chatty agent
//! churning most of the screen every frame, a suspend, a wedged emulator —
//! blocks input dispatch behind it. That is the freeze the incident log
//! shows as `render_ms=100…7000` with every other phase at zero.
//!
//! This moves the flush to a dedicated writer thread. The render thread
//! writes each frame's bytes into a [`ChannelWriter`] (an in-memory
//! `io::Write` that never blocks) and hands them off; the writer thread
//! owns the real stdout and does the blocking write. A slow terminal now
//! makes the *writer* fall behind — input never does.
//!
//! ## Coalescing without splitting a diff
//!
//! ratatui writes **diffs**, not full frames: each flush emits only the
//! cells that changed since the previous frame. Dropping a diff would
//! corrupt the screen permanently, so we must deliver every diff we
//! generate, in order. Instead of dropping frames *after* generating them,
//! the run loop skips rendering *before* generating one while the writer is
//! behind (see [`FrameWriter::pending_handle`]): ratatui's previous-frame state
//! then stays exactly the last frame we sent, so the next render is a
//! correct diff against it. Coalescing happens at the frame boundary, and
//! no diff is ever lost.
//!
//! Wiring (the `TerminalAdapter` whose backend is a `ChannelWriter`, the
//! run-loop skip gate, and teardown that drains then detaches on timeout so
//! a wedged terminal can't hang the exit) lives with the Model; this module
//! is the transport and is unit-tested in isolation over an in-memory sink.
//!
//! Opt-in behind `LAZYBOX_ASYNC_RENDER` for now: the writer is not yet the
//! *sole* owner of fd 1 (the signal-safe crash restore in `host_terminal`
//! still writes it directly), so async-by-default would regress the #211
//! stranded-shell guarantee. Making it default is the terminal-owner
//! architecture's job (#1045).

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long teardown waits for the writer to drain before giving up and
/// detaching it (#1090 finding #2). A wedged terminal can block the writer
/// inside `write_all` forever; joining it would hang the whole app on exit,
/// which is the exact freeze this feature exists to prevent. On a healthy
/// terminal the drain completes in microseconds, so this bound is never hit.
const TEARDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The single writer thread's `Sender`, published while async rendering is
/// active. Out-of-band terminal escapes — OSC notifications, mouse-capture
/// toggles — enqueue through here ([`enqueue_raw`]) instead of writing
/// stdout directly, so they can never interleave with a frame the writer is
/// mid-write on (the #296 regression a naive off-thread writer reintroduces).
/// Wrapped in a `Mutex` only because `Sender` is not `Sync`; contention is
/// nil (raw escapes are rare and frames use their own `Sender` clone).
static RAW_SINK: OnceLock<Mutex<Sender<WriterMsg>>> = OnceLock::new();

/// Route an out-of-band terminal escape (OSC banner, mouse-capture toggle)
/// to the terminal. In async mode it is queued on the same channel as
/// frames, so it is serialized behind the current frame and never splices
/// into a half-written one. In sync mode (no writer thread registered) it
/// writes stdout directly — the pre-#1090 behavior. Non-blocking either way.
pub(super) fn enqueue_raw(bytes: &[u8]) {
    if let Some(sink) = RAW_SINK.get() {
        if let Ok(tx) = sink.lock() {
            // Only claim the escape delivered if the writer actually took
            // it. If the thread is gone (send `Err`) fall through to a
            // direct write rather than silently dropping the escape — a lost
            // mouse-mode-off toggle would strand the terminal in reporting
            // mode, a lost OSC drops a banner.
            if tx.send(WriterMsg::Raw(bytes.to_vec())).is_ok() {
                return;
            }
        }
    }
    let mut out = io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Work item for the writer thread.
enum WriterMsg {
    /// One frame's worth of terminal bytes (a ratatui diff), to write and
    /// flush to the real terminal. Counted against the backpressure gate.
    Frame(Vec<u8>),
    /// An out-of-band escape (OSC / mouse), written verbatim and in order
    /// with the frames. Not counted against backpressure — it is small and
    /// must always go out (a dropped mouse-mode toggle wedges input).
    Raw(Vec<u8>),
    /// Stop the loop: drain any trailing work, flush, and exit.
    Shutdown,
}

/// The `io::Write` a ratatui `CrosstermBackend` writes into instead of
/// stdout. `write` accumulates bytes in memory; `flush` (which ratatui
/// calls once per completed frame) hands the accumulated frame to the
/// writer thread and returns immediately — it never touches the terminal,
/// so it can never block the render thread.
pub struct ChannelWriter {
    buf: Vec<u8>,
    tx: Sender<WriterMsg>,
    pending: Arc<AtomicUsize>,
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let frame = std::mem::take(&mut self.buf);
        // Count the frame as in-flight *before* handing it off, so the run
        // loop's skip gate can never observe a frame as drained early.
        let len = frame.len();
        self.pending.fetch_add(len, Ordering::AcqRel);
        if self.tx.send(WriterMsg::Frame(frame)).is_err() {
            // Writer thread is gone (teardown). Undo *this frame's*
            // accounting only — other frames may still be legitimately in
            // flight, so zeroing the whole counter would corrupt the gate.
            self.pending.fetch_sub(len, Ordering::AcqRel);
        }
        Ok(())
    }
}

/// Handle to the writer thread, held by the terminal adapter. `pending`
/// feeds the run loop's render-skip gate; [`shutdown`](Self::shutdown)
/// drains and joins so the terminal is left clean on exit.
pub(super) struct FrameWriter {
    pending: Arc<AtomicUsize>,
    tx: Sender<WriterMsg>,
    /// Signalled by the writer thread once it has drained and is about to
    /// exit. Teardown waits on this with a timeout instead of joining, so a
    /// wedged terminal can't hang the exit (#1090 finding #2).
    done: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl FrameWriter {
    /// A shared handle to the in-flight byte count, for the run loop's
    /// render-skip gate. The loop skips rendering a new frame while this is
    /// over its backlog cap, coalescing at the frame boundary (module docs).
    /// Handed to the `Model` at construction so the generic run loop can
    /// read it without knowing the adapter's concrete type.
    pub(super) fn pending_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.pending)
    }

    /// Signal the writer to drain and exit, waiting up to `timeout` for it.
    /// Called on teardown *before* the host-terminal restore sequence, so
    /// the last frames land before the alternate screen comes down.
    ///
    /// It **waits on a completion signal, not `join`** (#1090 finding #2):
    /// if the terminal is wedged the writer is blocked in `write_all` and a
    /// join would hang the app on exit — the very freeze this feature
    /// prevents. On timeout the thread is detached (the process is exiting;
    /// the OS reclaims it) and teardown proceeds. Idempotent.
    pub(super) fn shutdown_with_timeout(&mut self, timeout: Duration) {
        let _ = self.tx.send(WriterMsg::Shutdown);
        match self.done.recv_timeout(timeout) {
            // Drained cleanly: the thread is done, so joining is instant.
            Ok(()) => {
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
            // Wedged terminal or already gone: detach rather than hang.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                self.handle.take();
            }
        }
    }

    /// Teardown with the default drain timeout.
    pub(super) fn shutdown(&mut self) {
        self.shutdown_with_timeout(TEARDOWN_DRAIN_TIMEOUT);
    }

    /// Enqueue an out-of-band escape on this writer's own channel — the
    /// same path `enqueue_raw` takes in production, but without the global
    /// sink, so a test can assert Frame/Raw ordering deterministically.
    #[cfg(test)]
    fn send_raw_for_test(&self, bytes: &[u8]) {
        let _ = self.tx.send(WriterMsg::Raw(bytes.to_vec()));
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn the writer thread draining frames to `out` (real stdout in
/// production; an in-memory sink in tests). Returns the [`ChannelWriter`]
/// to install as the ratatui backend's sink and the [`FrameWriter`] handle
/// the adapter holds for the pending-gate and teardown.
pub(super) fn spawn<W: Write + Send + 'static>(mut out: W) -> (ChannelWriter, FrameWriter) {
    let (tx, rx): (Sender<WriterMsg>, Receiver<WriterMsg>) = channel();
    let (done_tx, done_rx) = channel::<()>();
    let pending = Arc::new(AtomicUsize::new(0));
    let pending_thread = Arc::clone(&pending);

    // Publish the sender so out-of-band escapes (OSC / mouse) share this one
    // channel — the single-writer invariant that keeps them from splicing
    // into a frame. `set` only takes the first writer; a process has one.
    let _ = RAW_SINK.set(Mutex::new(tx.clone()));

    let handle = std::thread::Builder::new()
        .name("lazybox-render-writer".to_string())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    WriterMsg::Frame(bytes) => {
                        let n = bytes.len();
                        // The blocking write — but on THIS thread, not the
                        // input/render thread.
                        let _ = out.write_all(&bytes);
                        let _ = out.flush();
                        pending_thread.fetch_sub(n, Ordering::AcqRel);
                    }
                    WriterMsg::Raw(bytes) => {
                        let _ = out.write_all(&bytes);
                        let _ = out.flush();
                    }
                    WriterMsg::Shutdown => break,
                }
            }
            // A Shutdown can race ahead of trailing work already queued;
            // drain it so the final screen state reaches the terminal.
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    WriterMsg::Frame(bytes) | WriterMsg::Raw(bytes) => {
                        let _ = out.write_all(&bytes);
                    }
                    WriterMsg::Shutdown => {}
                }
            }
            let _ = out.flush();
            // Tell teardown we finished draining (it waits on this, not a
            // join, so a wedged terminal that blocked us above simply times
            // the wait out instead of hanging the exit).
            let _ = done_tx.send(());
        })
        .expect("spawn lazybox render-writer thread");

    (
        ChannelWriter {
            buf: Vec::with_capacity(64 * 1024),
            tx: tx.clone(),
            pending: Arc::clone(&pending),
        },
        FrameWriter {
            pending,
            tx,
            done: done_rx,
            handle: Some(handle),
        },
    )
}

/// The `io::Write` the ratatui `CrosstermBackend` targets. `Sync` is the
/// pre-#1090 behavior — writes go straight to stdout on the render thread;
/// `Async` routes them through the off-thread [`ChannelWriter`]. Keeping
/// both behind one enum means a single `Model`/adapter type and a flag that
/// only changes the sink — the default (`Sync`) is byte-for-byte the old
/// path, so it carries no risk while the async path is validated.
pub enum RenderSink {
    Sync(io::Stdout),
    Async(ChannelWriter),
}

impl Write for RenderSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            RenderSink::Sync(out) => out.write(bytes),
            RenderSink::Async(out) => out.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            RenderSink::Sync(out) => out.flush(),
            RenderSink::Async(out) => out.flush(),
        }
    }
}

/// A `tuirealm::TerminalAdapter` whose backend writes through [`RenderSink`]
/// — direct stdout by default, or the off-thread writer when
/// `LAZYBOX_ASYNC_RENDER` is set. Replaces `CrosstermTerminalAdapter` as the
/// production adapter; the host-terminal *modes* (raw / alt screen / mouse /
/// paste / focus / kitty) are still owned by `host_terminal`, so this
/// adapter's mode methods are intentionally no-ops.
pub struct AsyncCrosstermAdapter {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<RenderSink>>,
    /// `Some` only in async mode; its `Drop` drains + joins the writer
    /// thread on teardown (the `Model` drops `terminal` before its
    /// `HostTerminalGuard`, so the last frames land before the alt screen
    /// comes down).
    writer: Option<FrameWriter>,
}

impl AsyncCrosstermAdapter {
    /// Build the production adapter. Direct-to-stdout (the pre-#1090 path)
    /// by **default**; `LAZYBOX_ASYNC_RENDER=1` opts into the off-thread
    /// writer, which keeps a slow terminal write off the input thread so
    /// keystrokes and modals never freeze behind it (#1090). Opt-in — not
    /// default — until the terminal-owner architecture makes the writer the
    /// sole owner of fd 1 (frames, OSC, mouse, setup/teardown, crash
    /// restore) and thus crash-safe (#1045); today the fatal-signal restore
    /// and the writer thread can both touch fd 1, so async-by-default would
    /// regress the #211 stranded-shell guarantee.
    pub(crate) fn new() -> io::Result<Self> {
        let (sink, writer) = if std::env::var_os("LAZYBOX_ASYNC_RENDER").is_some() {
            let (channel_writer, frame_writer) = spawn(io::stdout());
            (RenderSink::Async(channel_writer), Some(frame_writer))
        } else {
            (RenderSink::Sync(io::stdout()), None)
        };
        let backend = ratatui::backend::CrosstermBackend::new(sink);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal, writer })
    }

    /// Backpressure handle for the run loop's render-skip gate; `None`
    /// (sync mode) means the loop never skips.
    pub(crate) fn pending_handle(&self) -> Option<Arc<AtomicUsize>> {
        self.writer.as_ref().map(FrameWriter::pending_handle)
    }
}

impl tuirealm::terminal::TerminalAdapter for AsyncCrosstermAdapter {
    type Backend = ratatui::backend::CrosstermBackend<RenderSink>;

    fn enable_raw_mode(&mut self) -> tuirealm::terminal::TerminalResult<()> {
        Ok(())
    }
    fn disable_raw_mode(&mut self) -> tuirealm::terminal::TerminalResult<()> {
        Ok(())
    }
    fn enter_alternate_screen(&mut self) -> tuirealm::terminal::TerminalResult<()> {
        Ok(())
    }
    fn leave_alternate_screen(&mut self) -> tuirealm::terminal::TerminalResult<()> {
        Ok(())
    }
    fn enable_mouse_capture(&mut self) -> tuirealm::terminal::TerminalResult<()> {
        Ok(())
    }
    fn disable_mouse_capture(&mut self) -> tuirealm::terminal::TerminalResult<()> {
        Ok(())
    }
    fn raw_mut(&mut self) -> &mut ratatui::Terminal<Self::Backend> {
        &mut self.terminal
    }
    fn raw(&self) -> &ratatui::Terminal<Self::Backend> {
        &self.terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory stand-in for stdout that records everything written,
    /// shareable with the test thread so it can assert on the bytes the
    /// writer thread delivered.
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Every frame's bytes reach the sink, in order, and `pending` returns
    /// to zero once drained — the core transport contract.
    #[test]
    fn frames_reach_the_sink_in_order() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(Arc::clone(&recorded));
        let (mut writer, mut handle) = spawn(sink);

        for frame in [b"\x1b[Hhello".as_slice(), b" world", b"!"] {
            writer.write_all(frame).unwrap();
            writer.flush().unwrap();
        }
        // Drain + join guarantees all frames are written before we assert.
        let pending = handle.pending_handle();
        handle.shutdown();

        assert_eq!(recorded.lock().unwrap().as_slice(), b"\x1b[Hhello world!");
        assert_eq!(pending.load(Ordering::Acquire), 0);
    }

    /// `write` without `flush` sends nothing — a frame is delimited by the
    /// flush ratatui issues once the diff is complete, so a half-built
    /// frame is never handed off.
    #[test]
    fn write_without_flush_holds_the_frame() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(Arc::clone(&recorded));
        let (mut writer, mut handle) = spawn(sink);

        writer.write_all(b"partial").unwrap();
        // No flush yet: pending is still zero and nothing was sent.
        assert_eq!(handle.pending_handle().load(Ordering::Acquire), 0);
        assert!(recorded.lock().unwrap().is_empty());

        writer.flush().unwrap();
        handle.shutdown();
        assert_eq!(recorded.lock().unwrap().as_slice(), b"partial");
    }

    /// Out-of-band escapes (OSC / mouse) and frames share the one channel,
    /// so they are written strictly in submission order and a raw escape can
    /// never splice into a half-written frame (#1090 finding #1 / #296).
    #[test]
    fn raw_and_frames_write_in_submission_order() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(Arc::clone(&recorded));
        let (mut writer, mut handle) = spawn(sink);

        writer.write_all(b"[frameA]").unwrap();
        writer.flush().unwrap();
        handle.send_raw_for_test(b"<osc>");
        writer.write_all(b"[frameC]").unwrap();
        writer.flush().unwrap();
        handle.shutdown();

        assert_eq!(
            recorded.lock().unwrap().as_slice(),
            b"[frameA]<osc>[frameC]"
        );
    }

    /// Teardown must not hang when the terminal is wedged (#1090 finding
    /// #2): the writer is blocked in `write_all`, so `shutdown` times out
    /// and detaches rather than joining forever. Before the fix this test
    /// would hang the suite.
    #[test]
    fn shutdown_returns_when_the_terminal_is_wedged() {
        use std::sync::Condvar;

        struct BlockingSink(Arc<(Mutex<bool>, Condvar)>);
        impl Write for BlockingSink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let (lock, cvar) = &*self.0;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (mut writer, mut handle) = spawn(BlockingSink(Arc::clone(&gate)));
        writer.write_all(b"stuck").unwrap();
        writer.flush().unwrap();

        // Writer is now blocked in the sink. shutdown must return within the
        // (short) timeout instead of joining the blocked thread forever.
        let start = std::time::Instant::now();
        handle.shutdown_with_timeout(Duration::from_millis(50));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown blocked on a wedged terminal for {:?}",
            start.elapsed()
        );

        // Release so the detached thread can exit cleanly at test end.
        let (lock, cvar) = &*gate;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// The invariant (#1090 / #1045): a wedged terminal that is not
    /// accepting output must NOT block the render thread. This is the
    /// regression guard for the whole freeze class — if the flush ever
    /// moves back onto the render thread, the render thread would block
    /// inside `write` here and the frame enqueues below would hang.
    #[test]
    fn a_blocked_terminal_does_not_block_the_render_thread() {
        use std::sync::Condvar;

        /// A sink whose writes park until released — a wedged terminal.
        struct BlockingSink {
            gate: Arc<(Mutex<bool>, Condvar)>,
            recorded: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for BlockingSink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let (lock, cvar) = &*self.gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
                self.recorded.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = BlockingSink {
            gate: Arc::clone(&gate),
            recorded: Arc::clone(&recorded),
        };
        let (mut writer, mut handle) = spawn(sink);
        let pending = handle.pending_handle();

        // The writer thread is (about to be) parked in the blocked sink.
        // The render thread enqueues three frames — each returns without
        // blocking, or this test would hang forever.
        for frame in [b"a".as_slice(), b"bb", b"ccc"] {
            writer.write_all(frame).unwrap();
            writer.flush().unwrap();
        }
        // All three are queued: the wedged writer can't have drained any,
        // so `pending` holds every byte and nothing reached the sink.
        assert_eq!(pending.load(Ordering::Acquire), 1 + 2 + 3);
        assert!(recorded.lock().unwrap().is_empty());

        // Release the terminal: everything drains in order, thread joins.
        {
            let (lock, cvar) = &*gate;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        handle.shutdown();
        assert_eq!(recorded.lock().unwrap().as_slice(), b"abbccc");
        assert_eq!(pending.load(Ordering::Acquire), 0);
    }

    /// An empty flush is a no-op — it must not enqueue a zero-length frame
    /// or perturb the pending accounting.
    #[test]
    fn empty_flush_is_a_noop() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(Arc::clone(&recorded));
        let (mut writer, mut handle) = spawn(sink);

        writer.flush().unwrap();
        assert_eq!(handle.pending_handle().load(Ordering::Acquire), 0);
        handle.shutdown();
        assert!(recorded.lock().unwrap().is_empty());
    }
}
