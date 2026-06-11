//! Server-side PTY.
//!
//! The daemon doesn't render terminals — that's the client's job with
//! its own libghostty-vt. The daemon only needs to (1) spawn a PTY,
//! (2) stream raw bytes out to subscribers, (3) accept bytes in,
//! (4) resize, (5) know when the child exits. So this is deliberately
//! smaller than `lazybox-tui-term::TermSession` and critically it's
//! **Send-safe** — no libghostty pointers.
//!
//! Subscription model: one terminal can have N subscribers (e.g. a
//! local TUI plus a remote TUI watching the same daemon). Each new
//! subscription gets the ring-buffer replay first, then a broadcast
//! stream of new bytes. Dropped subscribers are cleaned up in the
//! main loop when `send` errors.

use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Mutex, Notify, broadcast, oneshot};

/// Ring-buffer capacity for per-terminal output replay. 64 KiB matches
/// a typical terminal scrollback and is enough to reconstruct the
/// visible screen after a client reconnects.
pub const REPLAY_RING_BYTES: usize = 64 * 1024;

/// Broadcast channel capacity. If a subscriber lags by more than this
/// many chunks it gets dropped with `RecvError::Lagged` — ring-buffer
/// replay on reconnect is how we recover that client.
pub const BROADCAST_CAPACITY: usize = 1024;

/// Capacity of the per-PTY write queue feeding the dedicated writer
/// thread. Generous for keystrokes + prompt injections; only a child
/// that stops draining its stdin (full kernel PTY buffer) can fill it.
pub const WRITE_QUEUE_CAPACITY: usize = 256;

/// How long `write()` is willing to wait for queue space before
/// reporting the PTY as stalled. Short — the caller is usually the
/// daemon serve path and must never wedge on a dead child.
const WRITE_ENQUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("PTY open: {0}")]
    Open(String),
    #[error("PTY spawn: {0}")]
    Spawn(String),
    #[error("PTY write: {0}")]
    Write(#[from] std::io::Error),
    #[error("PTY already closed")]
    Closed,
    /// The write queue stayed full past the enqueue timeout — the
    /// child has stopped draining stdin (wedged process, full kernel
    /// buffer). The write is dropped rather than blocking the caller.
    #[error("PTY write queue full — child not draining stdin")]
    WriteStalled,
}

/// One chunk of PTY output with its monotonic sequence number.
/// Carried on the broadcast channel so subscribers can detect gaps.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub seq: u64,
    pub bytes: Arc<[u8]>,
}

/// Server-side handle to a running PTY. `Send + Sync`.
pub struct DaemonPty {
    /// Bounded queue into the dedicated writer thread. The PTY's
    /// stdin writer is synchronous; calling `write_all` + `flush` on
    /// the tokio runtime blocked an OS worker thread whenever the
    /// kernel PTY buffer filled (wedged child) — which wedged the
    /// whole serve loop. The writer thread mirrors the reader-thread
    /// pattern: it owns the blocking IO, and `write()` is a bounded
    /// send that fails fast instead of blocking forever. The thread
    /// exits when the channel closes (DaemonPty dropped) or a write
    /// errors.
    writer_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Master end of the PTY, needed for resize.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Broadcast channel for live output. Subscribers get chunks with
    /// monotonic `seq` so replay+live can be stitched without dupes.
    output_tx: broadcast::Sender<OutputChunk>,
    /// Recent output, capped at `REPLAY_RING_BYTES`.
    ring: Arc<Mutex<ReplayRing>>,
    /// Set by the reader thread when the PTY reports EOF. Subscribers
    /// use this + `exit_code` to stop listening.
    finished: Arc<AtomicBool>,
    /// Notified once when the reader thread observes EOF. Lets async
    /// consumers (the spawn-handler output pump) wake up promptly
    /// without polling `finished`. The DaemonPty itself holds an
    /// Arc<...> which prevents the broadcast channel from closing
    /// based purely on dropping senders, so this is the actual
    /// "finished" signal for async tasks.
    finished_notify: Arc<Notify>,
    /// Filled exactly once when the child exits.
    exit_rx: Arc<Mutex<Option<oneshot::Receiver<Option<i32>>>>>,
    /// Latest assigned seq. Reader thread increments.
    last_seq: Arc<AtomicU64>,
    /// Captured at spawn time so `kill()` can SIGTERM the child even
    /// after `child` has been moved into the wait thread. `None` when
    /// portable-pty couldn't read the pid (rare; emits a warn).
    child_pid: Option<u32>,
}

/// Fixed-capacity byte ring. Writes overwrite the oldest bytes; reads
/// return a logical linear slice of everything currently stored.
#[derive(Debug)]
pub struct ReplayRing {
    buf: Vec<u8>,
    /// Capacity the ring enforces on the buffer's length.
    cap: usize,
    /// Total bytes ever written. The ring contains bytes
    /// `[total - buf.len(), total)`. Monotonic — useful for
    /// cross-checking seq numbers in tests.
    pub(crate) total_written: u64,
}

impl Default for ReplayRing {
    fn default() -> Self {
        Self::with_capacity(REPLAY_RING_BYTES)
    }
}

impl ReplayRing {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            total_written: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total_written += bytes.len() as u64;
        if bytes.len() >= self.cap {
            // Incoming burst alone exceeds capacity — keep only the tail.
            let tail_start = bytes.len() - self.cap;
            self.buf.clear();
            self.buf.extend_from_slice(&bytes[tail_start..]);
            return;
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.copy_within(excess.., 0);
            self.buf.truncate(self.cap);
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// A subscription to a `DaemonPty`'s output. Includes the replay so
/// the caller can reconstruct the screen, then the live stream for
/// everything after.
pub struct Subscription {
    pub replay: Vec<u8>,
    pub last_seq: u64,
    pub live: broadcast::Receiver<OutputChunk>,
}

impl DaemonPty {
    /// Spawn a command in a new PTY. `env` augments (does not replace)
    /// the parent environment except for `TERM` which we override to
    /// `xterm-256color` so agents render consistent colors.
    pub fn spawn(
        cmd: &[String],
        size: PtySize,
        cwd: Option<&PathBuf>,
        env: Vec<(String, String)>,
    ) -> Result<Self, PtyError> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(size)
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let program = cmd
            .first()
            .ok_or_else(|| PtyError::Spawn("empty command".into()))?;
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

        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let child_pid = child.process_id();
        if child_pid.is_none() {
            tracing::warn!("portable-pty did not report child pid; kill() will be a no-op");
        }
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        // Writer thread: drains the bounded queue into the PTY's
        // blocking stdin writer. Dedicated thread (like the reader
        // below) so a full kernel PTY buffer can only stall this
        // thread, never a tokio worker. `blocking_recv` returns
        // `None` when every sender is dropped (the DaemonPty), which
        // is the thread's exit signal.
        let (writer_tx, mut writer_rx) =
            tokio::sync::mpsc::channel::<Vec<u8>>(WRITE_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("lazybox-server-pty-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Some(bytes) = writer_rx.blocking_recv() {
                    if let Err(e) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                        tracing::warn!("PTY writer: {e}");
                        break;
                    }
                }
                // Receiver drops here; pending `send`s fail with
                // Closed, which `write()` maps to PtyError::Closed.
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let (output_tx, _) = broadcast::channel::<OutputChunk>(BROADCAST_CAPACITY);
        let ring = Arc::new(Mutex::new(ReplayRing::with_capacity(REPLAY_RING_BYTES)));
        let finished = Arc::new(AtomicBool::new(false));
        let finished_notify = Arc::new(Notify::new());
        let last_seq = Arc::new(AtomicU64::new(0));

        // Reader thread: blocks on PTY reads, fans bytes out to ring +
        // broadcast. Runs on std::thread because portable-pty's Read
        // impl is blocking.
        let reader_tx = output_tx.clone();
        let reader_ring = ring.clone();
        let reader_finished = finished.clone();
        let reader_notify = finished_notify.clone();
        let reader_seq = last_seq.clone();
        // Raw-byte capture for building the detector fixture corpus. When
        // `LAZYBOX_CAPTURE_PTY=<dir>` is set we dump every chunk — exactly
        // the bytes the state detector scrapes — to disk before any ring
        // or broadcast processing. Keyed by child pid so concurrent
        // sessions land in separate files. Off by default; never on a
        // production path.
        let mut capture = open_capture(child_pid);
        std::thread::Builder::new()
            .name("lazybox-server-pty".into())
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF — child exited
                        Ok(n) => {
                            if let Some(f) = capture.as_mut() {
                                let _ = f.write_all(&buf[..n]);
                            }
                            let bytes: Arc<[u8]> = Arc::from(&buf[..n]);
                            let seq = reader_seq.fetch_add(1, Ordering::SeqCst) + 1;
                            // Ring write uses blocking lock; this thread is
                            // dedicated so it's fine.
                            {
                                let mut r = reader_ring.blocking_lock();
                                r.push(&bytes);
                            }
                            // If no subscribers, broadcast returns error;
                            // we don't care — the ring holds the data.
                            let _ = reader_tx.send(OutputChunk { seq, bytes });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            tracing::warn!("PTY reader: {e}");
                            break;
                        }
                    }
                }
                reader_finished.store(true, Ordering::Release);
                reader_notify.notify_waiters();
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        // Exit watcher — blocking `wait` on another thread, forwards
        // the exit code through a oneshot so the daemon loop can await.
        let (exit_tx, exit_rx) = oneshot::channel::<Option<i32>>();
        std::thread::Builder::new()
            .name("lazybox-server-exit".into())
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => status.exit_code().try_into().ok(),
                    Err(e) => {
                        tracing::warn!("child.wait: {e}");
                        None
                    }
                };
                let _ = exit_tx.send(code);
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        Ok(Self {
            writer_tx,
            master: Arc::new(Mutex::new(pair.master)),
            output_tx,
            ring,
            finished,
            finished_notify,
            exit_rx: Arc::new(Mutex::new(Some(exit_rx))),
            last_seq,
            child_pid,
        })
    }

    /// A future that resolves once the PTY reader thread has observed
    /// EOF (the child exited). Always returns immediately if EOF has
    /// already been observed — async consumers don't need to race the
    /// notify against an existing finished flag.
    pub async fn wait_finished(&self) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        // Notified::notified() must be created before the atomic
        // re-check to avoid missing a notify that fires between the
        // load above and the await below.
        let notified = self.finished_notify.notified();
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// SIGTERM the child. Subsequent reads from the PTY will EOF
    /// once the child has actually exited, which the reader thread
    /// observes and uses to trip `finished` + close the broadcast
    /// channel — that's what causes the `TerminalExited` event to
    /// fire downstream. No-op if the pid wasn't captured at spawn.
    pub fn kill(&self) {
        let Some(pid) = self.child_pid else { return };
        #[cfg(unix)]
        unsafe {
            // SIGTERM rather than SIGKILL so the agent gets a chance
            // to clean up its session file / save state.
            libc::kill(pid as i32, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
        }
    }

    /// Fire up a subscription: the current ring snapshot + a live feed.
    pub async fn subscribe(&self) -> Subscription {
        let (replay, last_seq) = self.snapshot_only().await;
        let live = self.output_tx.subscribe();
        Subscription {
            replay,
            last_seq,
            live,
        }
    }

    /// Just the ring snapshot + last_seq, no new broadcast subscriber.
    /// Used by `Subscribe` snapshot path so reconnecting `--connect`
    /// clients can reconstruct their terminals without leaking a
    /// drainless broadcast receiver + pump task per snapshot call.
    pub async fn snapshot_only(&self) -> (Vec<u8>, u64) {
        let ring = self.ring.lock().await;
        (ring.snapshot(), self.last_seq.load(Ordering::SeqCst))
    }

    /// Queue bytes for the writer thread. Returns promptly in every
    /// case: a healthy PTY enqueues immediately; a wedged child (write
    /// queue full past `WRITE_ENQUEUE_TIMEOUT`) gets a `WriteStalled`
    /// error and the bytes are dropped — never an indefinite block on
    /// the runtime.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if self.finished.load(Ordering::Acquire) {
            return Err(PtyError::Closed);
        }
        match tokio::time::timeout(WRITE_ENQUEUE_TIMEOUT, self.writer_tx.send(bytes.to_vec())).await
        {
            Ok(Ok(())) => Ok(()),
            // Writer thread gone (write error or PTY torn down).
            Ok(Err(_)) => Err(PtyError::Closed),
            Err(_) => {
                tracing::warn!(
                    "PTY write queue full for {}ms — dropping {} byte write",
                    WRITE_ENQUEUE_TIMEOUT.as_millis(),
                    bytes.len()
                );
                Err(PtyError::WriteStalled)
            }
        }
    }

    pub async fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let m = self.master.lock().await;
        m.resize(size).map_err(|e| PtyError::Open(e.to_string()))
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Await the child's exit code. Returns None if the exit was
    /// unobservable (rare — `child.wait` error). Can only be called
    /// once per PTY; subsequent calls return None.
    pub async fn wait_exit(&self) -> Option<i32> {
        // Take the receiver out under the lock, then drop the guard
        // BEFORE awaiting the child's exit — holding the mutex across
        // that await would block every concurrent `wait_exit` caller
        // for the child's whole lifetime.
        let rx = self.exit_rx.lock().await.take()?;
        rx.await.ok().flatten()
    }

    /// Total bytes ever emitted by the PTY reader — NOT just what's
    /// retained in the ring. Used for byte-count metrics. For gap
    /// detection across reconnect, use `subscribe().last_seq` instead
    /// (seq counts chunks, not bytes).
    pub async fn total_written(&self) -> u64 {
        self.ring.lock().await.total_written
    }
}

/// Open a per-PTY capture file when `LAZYBOX_CAPTURE_PTY=<dir>` is set,
/// else `None`. The dir is created if missing; an unset var or any IO
/// error disables capture silently (a warn for the error) — capture is
/// a developer aid, never load-bearing. Keyed by child pid so two
/// concurrent sessions don't interleave into one file. When pid is
/// unknown we fall back to a fixed name and append, so a single session
/// still captures.
fn open_capture(child_pid: Option<u32>) -> Option<std::fs::File> {
    let dir = std::env::var_os("LAZYBOX_CAPTURE_PTY")?;
    let dir = PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("LAZYBOX_CAPTURE_PTY: create {} failed: {e}", dir.display());
        return None;
    }
    let name = match child_pid {
        Some(pid) => format!("pty-{pid}.bin"),
        None => "pty-unknown.bin".to_string(),
    };
    let path = dir.join(name);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => {
            tracing::info!(
                "LAZYBOX_CAPTURE_PTY: capturing raw PTY bytes to {}",
                path.display()
            );
            Some(f)
        }
        Err(e) => {
            tracing::warn!("LAZYBOX_CAPTURE_PTY: open {} failed: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use std::io::Write;

    /// Both the unset→`None` and set→`Some` cases live in ONE test on
    /// purpose. `set_var`/`remove_var` mutate process-global state, so
    /// splitting them into two `#[test]` fns would let `cargo test`'s
    /// thread pool run them concurrently — a libc env data race (the
    /// reason these calls are `unsafe` in edition 2024) and a flaky
    /// cross-test read. A single test serialises the mutations.
    ///
    /// SAFETY: no other test in this crate reads or writes
    /// `LAZYBOX_CAPTURE_PTY`, and within this test the calls are strictly
    /// ordered with no concurrent environment access; the prior value
    /// is restored on exit.
    #[test]
    fn open_capture_respects_lazybox_capture_pty_var() {
        let prior = std::env::var_os("LAZYBOX_CAPTURE_PTY");

        unsafe {
            std::env::remove_var("LAZYBOX_CAPTURE_PTY");
        }
        assert!(open_capture(Some(123)).is_none(), "unset var → no capture");

        let dir = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("LAZYBOX_CAPTURE_PTY", dir.path());
        }
        let mut f = open_capture(Some(4242)).expect("capture file opens when var is set");
        f.write_all(b"\x1b[2Khello").unwrap();
        f.flush().unwrap();
        drop(f);
        let captured = std::fs::read(dir.path().join("pty-4242.bin")).unwrap();
        assert_eq!(captured, b"\x1b[2Khello", "raw bytes written verbatim");

        unsafe {
            match prior {
                Some(p) => std::env::set_var("LAZYBOX_CAPTURE_PTY", p),
                None => std::env::remove_var("LAZYBOX_CAPTURE_PTY"),
            }
        }
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn empty_ring_snapshot_is_empty() {
        let r = ReplayRing::with_capacity(8);
        assert!(r.is_empty());
        assert_eq!(r.snapshot(), Vec::<u8>::new());
        assert_eq!(r.total_written, 0);
    }

    #[test]
    fn push_under_capacity_preserves_everything() {
        let mut r = ReplayRing::with_capacity(16);
        r.push(b"hello");
        r.push(b" world");
        assert_eq!(r.snapshot(), b"hello world");
        assert_eq!(r.total_written, 11);
    }

    #[test]
    fn push_at_capacity_is_exact() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abcde");
        assert_eq!(r.snapshot(), b"abcde");
        assert_eq!(r.total_written, 5);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn push_over_capacity_drops_oldest() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abcdef");
        // Incoming burst larger than capacity — only the last 5 bytes kept.
        assert_eq!(r.snapshot(), b"bcdef");
        assert_eq!(r.total_written, 6);
    }

    #[test]
    fn wrap_preserves_tail() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abc");
        r.push(b"def");
        r.push(b"g");
        // Total: 7 bytes written, last 5 retained: "cdefg".
        assert_eq!(r.snapshot(), b"cdefg");
        assert_eq!(r.total_written, 7);
    }

    #[test]
    fn large_burst_keeps_only_tail() {
        let mut r = ReplayRing::with_capacity(8);
        r.push(b"early-bytes-"); // 12 bytes, gets dropped entirely next step
        let burst: Vec<u8> = (b'A'..=b'Z').collect(); // 26 bytes
        r.push(&burst);
        // Only the last 8 bytes of the burst should remain.
        assert_eq!(r.snapshot(), b"STUVWXYZ");
        assert_eq!(r.total_written, 12 + 26);
    }

    #[test]
    fn many_small_pushes_then_wrap() {
        let mut r = ReplayRing::with_capacity(10);
        for b in b'0'..=b'9' {
            r.push(&[b]);
        }
        assert_eq!(r.snapshot(), b"0123456789");
        // One more push evicts '0'.
        r.push(b"X");
        assert_eq!(r.snapshot(), b"123456789X");
        assert_eq!(r.total_written, 11);
    }

    #[test]
    fn total_written_is_monotonic_and_exact() {
        let mut r = ReplayRing::with_capacity(4);
        r.push(b"a");
        assert_eq!(r.total_written, 1);
        r.push(b"bc");
        assert_eq!(r.total_written, 3);
        r.push(b"defghijk");
        assert_eq!(r.total_written, 11); // wraps; total still tracks real count
    }
}
