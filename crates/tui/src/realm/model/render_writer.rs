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
//! ## Sole fd-1 owner (#1045 / #1122)
//!
//! The writer thread is the **only** thing that writes fd 1 — frames, OSC,
//! mouse toggles, and the terminal-restore sequence all reach it. The
//! normal-context restore paths (clean exit, the panic hook, the
//! graceful-signal task) enqueue their reset through [`write_restore`], so it
//! serializes behind any in-flight frame on the one channel and can never
//! interleave with a half-written frame and strand the shell (#211).
//!
//! The lone exception is the fatal-signal handler (SIGSEGV / SIGABRT / …),
//! which cannot allocate, lock, or block — so it cannot use the channel. It
//! takes fd 1 back from the writer through the signal-safe [`muzzle_writer`]
//! handoff below and writes the reset with a raw `write(2)`; the handoff
//! guarantees the writer is not mid-write, so even that path cannot
//! interleave with a frame.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

// ── fd-1 ownership handoff for the fatal-signal restore (#1045) ────────────
//
// The writer thread is the sole writer of fd 1. Normal-context restores route
// their reset through the channel ([`write_restore`]), so they are naturally
// serialized behind any in-flight frame. The one path that cannot is the
// fatal-signal handler (`host_terminal::fatal::handle`) — SIGSEGV / SIGABRT /
// …, which don't unwind and cannot allocate, lock, or block, so it writes the
// reset sequence with a raw `write(2)` instead of enqueueing it.
//
// If that raw write lands while the writer thread is mid-`write()`, the reset
// bytes interleave with a half-written frame and the shell is left in the
// alternate screen / raw mode — the #211 stranding this whole feature must not
// regress.
//
// The handoff: the fatal path calls [`muzzle_writer`] first. It sets
// `MUZZLED` (the writer then drops any further frame instead of writing it)
// and bounded-spins until `IN_WRITE` clears, so the writer is guaranteed not
// to be inside a `write()` when the reset bytes go out. It touches only
// lock-free atomics and a bounded spin — no locks, no allocation, no
// unbounded blocking — so it is async-signal-safe. The bound is only ever hit
// on an already-wedged terminal (writer stuck in `write()`), where the restore
// is compromised regardless — never worse than the pre-existing sync path.

/// Upper bound on the muzzle spin. A healthy `write(2)` to fd 1 completes in
/// microseconds, so the spin almost always breaks on the first few
/// iterations; this cap only bounds the pathological "writer wedged inside
/// `write()`" case so a crash handler can never hang.
const MUZZLE_SPIN_LIMIT: u32 = 2_000_000;

/// The fd-1 handoff between the writer thread and a restore path. Two
/// lock-free flags: `muzzled` (a restore has claimed fd 1; the writer must
/// stop) and `in_write` (the writer is inside a `write()` right now).
struct Fd1Handoff {
    muzzled: AtomicBool,
    in_write: AtomicBool,
}

impl Fd1Handoff {
    const fn new() -> Self {
        Self {
            muzzled: AtomicBool::new(false),
            in_write: AtomicBool::new(false),
        }
    }

    fn is_muzzled(&self) -> bool {
        self.muzzled.load(Ordering::Acquire)
    }

    /// Announce an imminent fd-1 write and check the muzzle in one race-free
    /// step — the writer half of a Dekker handshake with [`Self::muzzle`].
    /// Store `in_write`, *then* load `muzzled`; [`Self::muzzle`] does the
    /// mirror (store `muzzled`, then load `in_write`). Both store→load pairs
    /// are `SeqCst`, so under the single total order the two sides can never
    /// *both* miss each other: if this returns `true` the caller owns fd 1 and
    /// `muzzle` is guaranteed to spin until [`Self::exit_write`]; if it returns
    /// `false` a restore path won the handoff and the caller must not write.
    ///
    /// The order matters. Checking `muzzled` *before* marking `in_write` (the
    /// naive way) leaves a window: a restore could set `muzzled`, see
    /// `in_write == false`, and start the reset in the gap between this check
    /// and the mark — then the frame writes over the reset and strands the
    /// shell (#211). Marking first closes it.
    fn try_begin_write(&self) -> bool {
        self.in_write.store(true, Ordering::SeqCst);
        if self.muzzled.load(Ordering::SeqCst) {
            self.in_write.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Simulate a writer wedged mid-write (`try_begin_write` without the
    /// matching `exit_write`), so a test can drive [`Self::muzzle`]'s bounded
    /// spin. Production marks `in_write` only through [`Self::try_begin_write`].
    #[cfg(test)]
    fn enter_write(&self) {
        self.in_write.store(true, Ordering::SeqCst);
    }

    fn exit_write(&self) {
        self.in_write.store(false, Ordering::Release);
    }

    /// Claim fd 1 for a direct write: stop the writer from starting another
    /// write, then bounded-spin until it is demonstrably not mid-write.
    /// Async-signal-safe — only lock-free atomics and a bounded spin. The
    /// bound is hit only when the writer is wedged inside `write()` (a
    /// terminal that stopped accepting output), where the reset is compromised
    /// either way — never worse than the sync path.
    ///
    /// `SeqCst` on the store and the spin's load is what makes the [Dekker]
    /// handshake with [`Self::try_begin_write`] airtight — see there.
    ///
    /// [Dekker]: Self::try_begin_write
    fn muzzle(&self) {
        self.muzzled.store(true, Ordering::SeqCst);
        let mut spins = 0u32;
        while self.in_write.load(Ordering::SeqCst) {
            if spins >= MUZZLE_SPIN_LIMIT {
                break;
            }
            spins += 1;
            std::hint::spin_loop();
        }
    }

    /// Seal fd 1 from the writer thread itself, after it has emitted the
    /// channel-routed reset: no later frame may clobber it. Unlike
    /// [`Self::muzzle`] there is no in-write spin — the writer is the caller,
    /// so it is by definition not concurrently inside a `write()`.
    fn seal(&self) {
        self.muzzled.store(true, Ordering::SeqCst);
    }
}

/// The live writer's handoff, published by [`spawn`] (first-wins, one writer
/// per process). Absent only until the writer spawns / in headless tests, so
/// [`muzzle_writer`] then pays nothing.
static FD1_HANDOFF: OnceLock<Arc<Fd1Handoff>> = OnceLock::new();

/// Take fd 1 back from the off-thread writer before the fatal-signal handler
/// writes the terminal-reset sequence with a raw `write(2)`. Async-signal-safe;
/// a no-op before the writer has spawned. See [`Fd1Handoff::muzzle`]. The
/// normal-context restore paths route through [`write_restore`] instead.
pub(crate) fn muzzle_writer() {
    if let Some(handoff) = FD1_HANDOFF.get() {
        handoff.muzzle();
    }
}

/// How long teardown waits for the writer to drain before giving up and
/// detaching it (#1090 finding #2). A wedged terminal can block the writer
/// inside `write_all` forever; joining it would hang the whole app on exit,
/// which is the exact freeze this feature exists to prevent. On a healthy
/// terminal the drain completes in microseconds, so this bound is never hit.
const TEARDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The single writer thread's `Sender`, published by [`spawn`]. Out-of-band
/// terminal escapes — OSC notifications, mouse-capture toggles — and the
/// normal-context terminal reset enqueue through here ([`enqueue_raw`] /
/// [`write_restore`]) instead of writing stdout directly, so they can never
/// interleave with a frame the writer is mid-write on (the #296 regression a
/// naive off-thread writer reintroduces). Wrapped in a `Mutex` only because
/// `Sender` is not `Sync`; contention is nil (raw escapes are rare and frames
/// use their own `Sender` clone).
static RAW_SINK: OnceLock<Mutex<Sender<WriterMsg>>> = OnceLock::new();

/// Route an out-of-band terminal escape (OSC banner, mouse-capture toggle)
/// to the terminal. It is queued on the same channel as frames, so it is
/// serialized behind the current frame and never splices into a half-written
/// one. Non-blocking.
pub(crate) fn enqueue_raw(bytes: &[u8]) {
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

/// Route the terminal-reset sequence through the writer thread — the sole
/// fd-1 owner — so it serializes behind any in-flight frame and can never
/// interleave with one (#1045 / #1122). The writer emits the bytes, then seals
/// fd 1 so no later frame can clobber the reset. Blocks until the writer has
/// emitted it — bounded, so a wedged terminal can't hang teardown — so the
/// reset reaches the host before a panic message or the returning shell prompt.
///
/// Used by every normal-context restore (clean exit, the panic hook, the
/// graceful-signal task). The fatal-signal handler cannot allocate or block,
/// so it takes the signal-safe [`muzzle_writer`] + raw-`write(2)` path instead.
pub(crate) fn write_restore(bytes: &[u8]) {
    if let Some(sink) = RAW_SINK.get() {
        if let Ok(tx) = sink.lock() {
            let (ack_tx, ack_rx) = channel();
            if tx
                .send(WriterMsg::Restore {
                    bytes: bytes.to_vec(),
                    ack: ack_tx,
                })
                .is_ok()
            {
                let _ = ack_rx.recv_timeout(TEARDOWN_DRAIN_TIMEOUT);
                return;
            }
        }
    }
    // The writer thread is gone (its `Drop` already tore it down) or was never
    // spawned (a headless test model). No other fd-1 writer exists, so a direct
    // write is safe and can't interleave.
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
    /// The terminal-reset sequence, routed here so the writer stays the sole
    /// fd-1 owner (#1122). Written in order behind any queued frame, after
    /// which fd 1 is sealed so no later frame can clobber the reset; `ack`
    /// signals the enqueuer once the bytes are out (it blocks on it so the
    /// reset lands before a panic message / the shell prompt).
    Restore { bytes: Vec<u8>, ack: Sender<()> },
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
    /// This writer's fd-1 handoff (also published to the global for the
    /// signal handler). Retained so tests can drive the muzzle against this
    /// writer without touching the process-global; production reaches it only
    /// through the global, so the field is read on the test path alone.
    #[cfg_attr(not(test), allow(dead_code))]
    handoff: Arc<Fd1Handoff>,
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

    /// Enqueue a channel-routed terminal reset on this writer's own channel —
    /// the same path `write_restore` takes in production — and return the ack
    /// receiver so a test can wait until the writer has emitted it.
    #[cfg(test)]
    fn send_restore_for_test(&self, bytes: &[u8]) -> Receiver<()> {
        let (ack_tx, ack_rx) = channel();
        let _ = self.tx.send(WriterMsg::Restore {
            bytes: bytes.to_vec(),
            ack: ack_tx,
        });
        ack_rx
    }

    /// This writer's fd-1 handoff, so a test can muzzle it directly (the
    /// process-global one the signal handler uses is first-wins and shared
    /// across the suite, so tests drive this per-writer clone instead).
    #[cfg(test)]
    fn handoff_for_test(&self) -> Arc<Fd1Handoff> {
        Arc::clone(&self.handoff)
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
    // The fd-1 handoff for the crash/panic restore paths (#1045). Published
    // to the global so the signal handler's `muzzle_writer` can reach it
    // (first-wins — one writer per process); the writer thread keeps its own
    // clone so tests can exercise the handoff without the global.
    let handoff = Arc::new(Fd1Handoff::new());
    let _ = FD1_HANDOFF.set(Arc::clone(&handoff));
    let handoff_thread = Arc::clone(&handoff);

    let handle = std::thread::Builder::new()
        .name("lazybox-render-writer".to_string())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    WriterMsg::Frame(bytes) => {
                        let n = bytes.len();
                        // Claim fd 1 for this write, unless a restore path
                        // (crash / panic) already took it — then drop the frame
                        // rather than interleave with the reset sequence it's
                        // about to write (#1045). `try_begin_write` marks
                        // `in_write` *before* checking the muzzle, so `muzzle`
                        // can never slip its reset between our check and our
                        // write. Either way still account the bytes as drained
                        // so the backpressure gate can't stall. The blocking
                        // write is on THIS thread, not the input/render thread.
                        if !handoff_thread.try_begin_write() {
                            pending_thread.fetch_sub(n, Ordering::AcqRel);
                            continue;
                        }
                        let _ = out.write_all(&bytes);
                        let _ = out.flush();
                        handoff_thread.exit_write();
                        pending_thread.fetch_sub(n, Ordering::AcqRel);
                    }
                    WriterMsg::Raw(bytes) => {
                        if !handoff_thread.try_begin_write() {
                            continue;
                        }
                        let _ = out.write_all(&bytes);
                        let _ = out.flush();
                        handoff_thread.exit_write();
                    }
                    WriterMsg::Restore { bytes, ack } => {
                        // The authorized terminal reset. Claim fd 1 through the
                        // same handoff as a frame so a fatal-signal muzzle
                        // racing us waits rather than interleaving; if the fatal
                        // path already claimed fd 1 it has emitted the reset
                        // itself, so skip ours. Either way seal fd 1 so no later
                        // frame can clobber the reset, then ack the enqueuer.
                        if handoff_thread.try_begin_write() {
                            let _ = out.write_all(&bytes);
                            let _ = out.flush();
                            handoff_thread.exit_write();
                        }
                        handoff_thread.seal();
                        let _ = ack.send(());
                    }
                    WriterMsg::Shutdown => break,
                }
            }
            // A Shutdown can race ahead of trailing work already queued;
            // drain it so the final screen state reaches the terminal —
            // unless a restore path muzzled us (panic teardown: the reset
            // sequence is already on fd 1, and replaying these queued frames
            // would clobber it). On the clean-exit path muzzle is unset and
            // this drains normally.
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    WriterMsg::Frame(bytes) | WriterMsg::Raw(bytes) => {
                        if !handoff_thread.is_muzzled() {
                            let _ = out.write_all(&bytes);
                        }
                    }
                    WriterMsg::Restore { bytes, ack } => {
                        if !handoff_thread.is_muzzled() {
                            let _ = out.write_all(&bytes);
                            handoff_thread.seal();
                        }
                        let _ = ack.send(());
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
            handoff,
        },
    )
}

/// A `tuirealm::TerminalAdapter` whose backend writes through the off-thread
/// [`ChannelWriter`], so a slow / wedged terminal write can never freeze the
/// input thread (#1090). Replaces `CrosstermTerminalAdapter` as the production
/// adapter; the host-terminal *modes* (raw / alt screen / mouse / paste /
/// focus / kitty) are still owned by `host_terminal`, so this adapter's mode
/// methods are intentionally no-ops.
pub struct AsyncCrosstermAdapter {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<ChannelWriter>>,
    /// Its `Drop` drains + joins the writer thread on teardown. The `Model`
    /// drops its `HostTerminalGuard` before `terminal`, so the guard's
    /// channel-routed reset ([`write_restore`]) reaches the still-live writer
    /// and lands after the final frame, before this drains and joins it.
    writer: FrameWriter,
}

impl AsyncCrosstermAdapter {
    /// Build the production adapter. The off-thread writer is the sole fd-1
    /// owner (#1045): the flush runs on its own thread, so a slow / wedged
    /// terminal can never freeze the input thread (the #1090 freeze), and every
    /// fd-1 write — frames, OSC/mouse, and the terminal reset — is serialized
    /// through it, so nothing interleaves with a half-written frame to strand
    /// the shell (#211).
    pub(crate) fn new() -> io::Result<Self> {
        let (channel_writer, writer) = spawn(io::stdout());
        let backend = ratatui::backend::CrosstermBackend::new(channel_writer);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal, writer })
    }

    /// Backpressure handle for the run loop's render-skip gate.
    pub(crate) fn pending_handle(&self) -> Arc<AtomicUsize> {
        self.writer.pending_handle()
    }
}

impl tuirealm::terminal::TerminalAdapter for AsyncCrosstermAdapter {
    type Backend = ratatui::backend::CrosstermBackend<ChannelWriter>;

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

    /// #1045: once a restore path muzzles the writer (it has claimed fd 1 for
    /// a crash / panic reset), the writer must stop writing — no further frame
    /// may reach fd 1 and interleave with the reset sequence.
    #[test]
    fn a_muzzled_writer_stops_touching_fd1() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(Arc::clone(&recorded));
        let (mut writer, mut handle) = spawn(sink);
        let pending = handle.pending_handle();
        let handoff = handle.handoff_for_test();

        // A normal frame writes through.
        writer.write_all(b"[before]").unwrap();
        writer.flush().unwrap();
        while pending.load(Ordering::Acquire) != 0 {
            std::hint::spin_loop();
        }
        assert_eq!(recorded.lock().unwrap().as_slice(), b"[before]");

        // A restore path claims fd 1.
        handoff.muzzle();
        assert!(handoff.is_muzzled());

        // Anything enqueued now is dropped by the writer (accounted as
        // drained so the gate can't stall), never written to fd 1.
        writer.write_all(b"[after-muzzle]").unwrap();
        writer.flush().unwrap();
        while pending.load(Ordering::Acquire) != 0 {
            std::hint::spin_loop();
        }
        assert_eq!(
            recorded.lock().unwrap().as_slice(),
            b"[before]",
            "a muzzled writer must not write to fd 1",
        );
        // The muzzled frame was already drained above, so shutdown's drain
        // loop has nothing left to flush — no race.
        handle.shutdown();
        assert_eq!(recorded.lock().unwrap().as_slice(), b"[before]");
    }

    /// #1122 acceptance: a queued frame and a fatal-signal restore never
    /// interleave. Models the fatal path exactly — a frame is *in flight* on
    /// fd 1 and another is *queued* behind it when the signal handler claims
    /// fd 1 via `muzzle`. The muzzle must (a) block until the in-flight frame's
    /// write completes, so the reset can't splice into it, and (b) leave the
    /// queued frame dropped, so it can't land after the reset. The recorded
    /// fd-1 byte stream is therefore exactly `[in-flight frame][reset]` — the
    /// queued frame never appears, and the reset never appears mid-frame.
    #[test]
    fn a_queued_frame_and_a_fatal_restore_never_interleave() {
        use std::sync::Condvar;

        /// A sink that signals when a write *starts*, then parks until
        /// released — lets the test pin the writer mid-`write()` (in_write set)
        /// so `muzzle` genuinely has an in-flight frame to wait out.
        struct GatedSink {
            recorded: Arc<Mutex<Vec<u8>>>,
            entered: Arc<(Mutex<bool>, Condvar)>,
            release: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Write for GatedSink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                {
                    let (lock, cvar) = &*self.entered;
                    *lock.lock().unwrap() = true;
                    cvar.notify_all();
                }
                let (lock, cvar) = &*self.release;
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

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let sink = GatedSink {
            recorded: Arc::clone(&recorded),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        };
        let (mut writer, mut handle) = spawn(sink);
        let handoff = handle.handoff_for_test();

        // Frame 1 goes in flight: the writer claims fd 1 and parks inside the
        // sink's `write` (in_write == true).
        writer.write_all(b"[in-flight]").unwrap();
        writer.flush().unwrap();
        {
            let (lock, cvar) = &*entered;
            let mut started = lock.lock().unwrap();
            while !*started {
                started = cvar.wait(started).unwrap();
            }
        }

        // Frame 2 is queued behind it.
        writer.write_all(b"[queued]").unwrap();
        writer.flush().unwrap();

        // The fatal-signal handler claims fd 1. `muzzle` sets the flag, then
        // spins on in_write — it must not return while frame 1 is mid-write.
        let muzzle_thread = {
            let handoff = Arc::clone(&handoff);
            std::thread::spawn(move || handoff.muzzle())
        };
        while !handoff.is_muzzled() {
            std::hint::spin_loop();
        }
        // Muzzle is set and spinning; the reset must not be on the wire yet —
        // it can only follow the in-flight frame.
        assert!(
            recorded.lock().unwrap().is_empty(),
            "the reset must wait for the in-flight frame to finish"
        );

        // Let frame 1 finish. in_write clears, so muzzle returns; the writer
        // then reaches frame 2 and drops it (muzzled).
        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        muzzle_thread.join().unwrap();

        // Now the handler writes the reset directly, as `fatal::handle` does.
        recorded.lock().unwrap().extend_from_slice(b"[reset]");
        handle.shutdown();

        // Exactly the in-flight frame then the reset — the queued frame never
        // reached fd 1, and the reset never spliced into the frame.
        assert_eq!(recorded.lock().unwrap().as_slice(), b"[in-flight][reset]");
    }

    /// #1122: the normal-context reset routes through the writer channel
    /// (`write_restore`) — it lands after any queued frame, then seals fd 1 so
    /// a later frame can't clobber it, and the ack fires once it is out.
    #[test]
    fn channel_routed_restore_lands_after_frames_then_seals() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(Arc::clone(&recorded));
        let (mut writer, mut handle) = spawn(sink);

        // A frame, then the reset, then a frame that must be dropped.
        writer.write_all(b"[frame]").unwrap();
        writer.flush().unwrap();
        let ack = handle.send_restore_for_test(b"[reset]");
        ack.recv_timeout(Duration::from_secs(5))
            .expect("restore acked once written");

        writer.write_all(b"[after-reset]").unwrap();
        writer.flush().unwrap();
        handle.shutdown();

        assert_eq!(
            recorded.lock().unwrap().as_slice(),
            b"[frame][reset]",
            "the reset lands after the frame and seals fd 1 against later frames",
        );
    }

    /// The muzzle returns immediately when the writer is not mid-write — the
    /// common crash case, where the reset sequence then goes out cleanly.
    #[test]
    fn muzzle_returns_at_once_when_writer_is_idle() {
        let handoff = Fd1Handoff::new();
        let start = std::time::Instant::now();
        handoff.muzzle();
        assert!(handoff.is_muzzled());
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "muzzle spun despite the writer being idle: {:?}",
            start.elapsed(),
        );
    }

    /// The muzzle is *bounded*: if the writer is wedged inside `write()`
    /// (a terminal that stopped draining), the spin gives up rather than
    /// hanging the crash handler forever. The reset is compromised in that
    /// case either way — but the handler never hangs.
    #[test]
    fn muzzle_is_bounded_when_the_writer_is_stuck_in_write() {
        let handoff = Fd1Handoff::new();
        handoff.enter_write(); // simulate a writer wedged mid-write
        let start = std::time::Instant::now();
        handoff.muzzle();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "muzzle failed to bound its spin on a wedged writer: {:?}",
            start.elapsed(),
        );
        assert!(handoff.is_muzzled());
    }

    /// The writer half of the handoff: once muzzled, `try_begin_write` refuses
    /// the write and leaves `in_write` clear, so a restore path that already
    /// won the muzzle never waits on a write that won't happen. Marking
    /// `in_write` before checking `muzzled` (both `SeqCst`) is what closes the
    /// interleave window — a restore can't slip its reset between the writer's
    /// muzzle-check and its write (#1045 / #211).
    #[test]
    fn try_begin_write_refuses_once_muzzled() {
        let handoff = Fd1Handoff::new();
        // Before any muzzle the writer claims fd 1 and must release it.
        assert!(handoff.try_begin_write());
        handoff.exit_write();

        handoff.muzzle();
        assert!(
            !handoff.try_begin_write(),
            "a muzzled handoff must refuse the write",
        );
        assert!(
            !handoff.in_write.load(Ordering::SeqCst),
            "a refused write must leave in_write clear so muzzle can't hang",
        );
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
