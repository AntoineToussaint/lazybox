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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Mutex, Notify, broadcast, watch};

/// Ring-buffer capacity for per-terminal output replay.
///
/// A client (re)attaching or recovering after a restart rebuilds its
/// entire VT grid — scrollback included — purely from this snapshot, so
/// the ring has to carry the history, not just the visible screen. The
/// old 64 KiB was sized for screen reconstruction; it left recovered
/// sessions with effectively nothing to scroll back through, because a
/// live agent's redraw churn (spinners, full-screen repaints) inflates
/// the byte stream far past the lines it ultimately leaves in
/// scrollback, so 64 KiB of raw bytes spans only a screenful or two of
/// real output.
///
/// 2 MiB carries enough raw history for a reattaching client to
/// reconstruct a scrollback depth on par with a session that streamed
/// live, even under that churn. Perfectly reconstructing arbitrarily
/// deep history would need on-disk persistence; this trades a bounded
/// slice of per-terminal daemon memory (paid only as output accrues)
/// for parity with the live experience.
pub const REPLAY_RING_BYTES: usize = 2 * 1024 * 1024;

/// On-disk scrollback contract: bytes of raw output retained per
/// persistent terminal (see [`DaemonPty::spawn_persistent`]).
///
/// The in-memory ring is destroyed when the daemon exits, so a terminal
/// whose child is respawned after a restart comes back with an empty
/// grid. A persistent terminal mirrors every output byte into a
/// per-session file and seeds the fresh PTY's replay from it, so restart
/// replay reconstructs real history rather than a blank screen. Sized to
/// match [`REPLAY_RING_BYTES`] so the persisted depth tracks what a live
/// reattaching client would rebuild from the ring.
pub const SCROLLBACK_PERSIST_BYTES: usize = REPLAY_RING_BYTES;

/// Compaction trigger for the on-disk scrollback file. Appends are cheap
/// and unbounded until the file crosses this, at which point it is
/// rewritten down to its last [`SCROLLBACK_PERSIST_BYTES`]. Twice the
/// retained size so compaction is amortized (one rewrite per
/// `SCROLLBACK_PERSIST_BYTES` appended) rather than per write.
const SCROLLBACK_COMPACT_BYTES: u64 = SCROLLBACK_PERSIST_BYTES as u64 * 2;

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

/// Signal numbers, spelled as plain i32s so the non-unix stub build
/// (where the `libc` constants aren't referenced) still compiles.
#[cfg(unix)]
const SIGTERM_CODE: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const SIGTERM_CODE: i32 = 15;
#[cfg(unix)]
const SIGKILL_CODE: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGKILL_CODE: i32 = 9;

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
    /// Holds the child's exit code once the watcher thread observes it
    /// (`None` until then). A `watch` rather than a `oneshot` so
    /// `wait_exit` is repeatable and safe to call concurrently — the
    /// tmux backend relies on calling it more than once.
    exit_watch: watch::Receiver<Option<Option<i32>>>,
    /// Latest assigned seq. Reader thread increments.
    last_seq: Arc<AtomicU64>,
    /// Durable reattach seed (e.g. tmux capture-pane history handed in
    /// at spawn). Kept OUT of the evictable replay ring: as ring chunk
    /// 1 it was silently evicted once live churn wrapped the ring, so
    /// the first resync/snapshot after that rebuilt clients WITHOUT
    /// their reattach scrollback (#420). `snapshot_only` prepends it to
    /// every ring snapshot instead, so it survives arbitrary churn.
    /// Empty when the PTY was spawned unseeded. Bounded by the
    /// backend's capture (tmux `history-limit` lines) and held for the
    /// PTY's lifetime.
    seed: Arc<[u8]>,
    /// Captured at spawn time so `kill()` can SIGTERM the child even
    /// after `child` has been moved into the wait thread. `None` when
    /// portable-pty couldn't read the pid (rare; emits a warn).
    child_pid: Option<u32>,
}

/// Fixed-capacity byte ring. Writes overwrite the oldest bytes; reads
/// return a logical linear slice of everything currently stored.
///
/// A true circular buffer: once storage reaches `cap`, `push` writes
/// over the oldest region in place and advances `head`. The previous
/// implementation kept the buffer linear with a `copy_within` over the
/// whole (2 MiB) buffer per ≤8 KiB chunk — ~256× write amplification
/// under the same lock `snapshot`/`subscribe` contend on.
#[derive(Debug)]
pub struct ReplayRing {
    /// Storage. Grows with content up to `cap` (an idle terminal never
    /// holds the full multi-MiB budget), then stays at `cap` and is
    /// treated circularly with `head` marking the oldest byte.
    buf: Vec<u8>,
    /// Capacity the ring enforces on the buffer's length.
    cap: usize,
    /// Index in `buf` of the oldest stored byte. Invariant: `head == 0`
    /// until the ring first fills (`buf.len() < cap` ⇒ `head == 0`).
    head: usize,
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
            // Grow on demand up to `cap` rather than reserving it all
            // upfront — an idle terminal shouldn't hold the full
            // (now multi-MiB) budget before it has emitted anything.
            buf: Vec::new(),
            cap,
            head: 0,
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
            self.head = 0;
            return;
        }
        let mut bytes = bytes;
        if self.buf.len() < self.cap {
            // Fill phase: storage still growing toward cap; head is 0.
            let room = self.cap - self.buf.len();
            let take = room.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if bytes.is_empty() {
                return;
            }
            // Storage just reached cap; the remainder overwrites from
            // head == 0 below.
        }
        // Overwrite phase: `buf.len() == cap`. Write over the oldest
        // region starting at `head`, wrapping at most once (the
        // burst-exceeds-cap case was handled above, so `bytes.len() <
        // cap`).
        let n = bytes.len();
        let first = (self.cap - self.head).min(n);
        self.buf[self.head..self.head + first].copy_from_slice(&bytes[..first]);
        if first < n {
            self.buf[..n - first].copy_from_slice(&bytes[first..]);
        }
        self.head = (self.head + n) % self.cap;
    }

    /// Everything currently stored, oldest byte first, assembled into
    /// one contiguous allocation on demand.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.buf.len());
        self.snapshot_into(&mut out);
        out
    }

    /// Append everything currently stored, oldest byte first, to `out`
    /// — the allocation-free form of [`Self::snapshot`] for callers
    /// assembling a larger replay (the durable seed prefix, #420).
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.buf[self.head..]);
        out.extend_from_slice(&self.buf[..self.head]);
    }

    /// Snapshot suitable for seeding a VT reconstruction, appended to
    /// `out`. Identical to [`Self::snapshot_into`] while the ring is
    /// complete: nothing has been evicted, so the first stored byte is
    /// the terminal's true clean start.
    ///
    /// Once the ring has wrapped, its oldest retained byte can fall in
    /// the middle of an escape / UTF-8 sequence. Replaying from there
    /// mis-parses that leading partial sequence as ground-state text —
    /// stray glyphs and wrong SGR/cursor state on the first reconstructed
    /// rows of scrollback. So for a wrapped ring this drops the partial
    /// leading line, starting the replay on a clean line boundary. An
    /// escape/UTF-8 sequence never spans a raw `\n`, so the byte after the
    /// first newline is never mid-sequence. This is the same clean-baseline
    /// guard [`read_scrollback_tail`] applies to the on-disk seed.
    pub fn replay_snapshot_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        self.snapshot_into(out);
        if self.is_complete() {
            return;
        }
        if let Some(rel) = out[start..].iter().position(|&b| b == b'\n') {
            out.drain(start..=start + rel);
        }
    }

    /// Owned form of [`Self::replay_snapshot_into`].
    pub fn replay_snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.buf.len());
        self.replay_snapshot_into(&mut out);
        out
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// True while the ring still contains the entire byte stream from
    /// the terminal's clean starting state. Once any prefix is evicted,
    /// the retained tail is useful for diagnostics/exit summaries but is
    /// not safe as an authoritative VT reset.
    pub fn is_complete(&self) -> bool {
        self.total_written == self.buf.len() as u64
    }
}

/// Append-backed on-disk mirror of a terminal's output, keeping the
/// most recent [`SCROLLBACK_PERSIST_BYTES`] durable across daemon
/// restarts. Owned by the reader thread, so its blocking file IO never
/// touches the async runtime — the same pattern as the reader ring and
/// the `LAZYBOX_CAPTURE_PTY` capture.
struct ScrollbackLog {
    file: std::fs::File,
    path: PathBuf,
    /// Bytes appended since the last compaction. Tracked rather than
    /// re-`stat`'d per write so the hot path stays a single `write_all`.
    len: u64,
}

impl ScrollbackLog {
    /// Open (creating parents) the append-mode log at `path`, seeding
    /// `len` from any bytes a prior run left behind. `None` on any IO
    /// error — persistence is best-effort and must never fail a spawn.
    fn open(path: PathBuf) -> Option<Self> {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                "scrollback persist: create {} failed: {e}",
                parent.display()
            );
            return None;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                Some(Self { file, path, len })
            }
            Err(e) => {
                tracing::warn!("scrollback persist: open {} failed: {e}", path.display());
                None
            }
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if let Err(e) = self.file.write_all(bytes) {
            tracing::warn!(
                "scrollback persist: write {} failed: {e}",
                self.path.display()
            );
            return;
        }
        self.len += bytes.len() as u64;
        if self.len > SCROLLBACK_COMPACT_BYTES {
            self.compact();
        }
    }

    /// Rewrite the file down to its last [`SCROLLBACK_PERSIST_BYTES`] so
    /// it can't grow without bound. Best-effort: a failed rewrite leaves
    /// the (larger but correct) file in place and retries next trigger.
    fn compact(&mut self) {
        let tail = read_scrollback_tail(&self.path, SCROLLBACK_PERSIST_BYTES);
        let tmp = self.path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, &tail) {
            tracing::warn!(
                "scrollback persist: compact write {} failed: {e}",
                tmp.display()
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            tracing::warn!(
                "scrollback persist: compact rename {} failed: {e}",
                self.path.display()
            );
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        // Reopen the append handle onto the freshly compacted inode; the
        // old handle still pointed at the now-unlinked original.
        match std::fs::OpenOptions::new().append(true).open(&self.path) {
            Ok(file) => {
                self.file = file;
                self.len = tail.len() as u64;
            }
            Err(e) => {
                tracing::warn!(
                    "scrollback persist: reopen {} after compact failed: {e}",
                    self.path.display()
                );
            }
        }
    }
}

/// Read the last `max` bytes of the scrollback file, trimmed to start at
/// a line boundary when the file was longer than `max`. The trim drops a
/// partial leading line so the retained head is a clean VT baseline
/// rather than the middle of a UTF-8 / escape sequence — the same
/// property tmux's line-oriented `capture-pane` seed has. Empty on any IO
/// error or a missing file (a fresh session has no prior scrollback).
fn read_scrollback_tail(path: &Path, max: usize) -> Vec<u8> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let start = meta.len().saturating_sub(max as u64);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    if start > 0
        && let Some(nl) = buf.iter().position(|&b| b == b'\n')
    {
        buf.drain(..=nl);
    }
    buf
}

/// A subscription to a `DaemonPty`'s output. Includes the replay so
/// the caller can reconstruct the screen, then the live stream for
/// everything after.
pub struct Subscription {
    pub replay: Vec<u8>,
    pub replay_complete: bool,
    pub last_seq: u64,
    pub live: broadcast::Receiver<OutputChunk>,
}

impl DaemonPty {
    /// Spawn a command in a new PTY. `env` augments (does not replace)
    /// the parent environment except for `TERM`/`COLORTERM`, which we
    /// override to `xterm-256color` + `truecolor` so agents render
    /// consistent colors: modern TUIs (Codex among them) gate full
    /// color on `COLORTERM=truecolor`, and with `TERM` alone many fall
    /// back to degraded or no color (#421). The vendored libghostty-vt
    /// parser handles 24-bit SGR regardless of the hosting terminal.
    ///
    /// `initial` seeds the replay: a (re)attaching client rebuilds its
    /// VT grid — scrollback included — purely from the snapshot, so a
    /// caller with history that predates this PTY (e.g. tmux scrollback
    /// that survived a daemon restart) hands it in here to be replayed
    /// ahead of the live stream. Stored in a durable slot outside the
    /// evictable ring (#420) and prepended to every snapshot. Pass
    /// `&[]` when there's nothing to seed.
    pub fn spawn(
        cmd: &[String],
        size: PtySize,
        cwd: Option<&PathBuf>,
        env: Vec<(String, String)>,
        initial: &[u8],
    ) -> Result<Self, PtyError> {
        Self::spawn_inner(cmd, size, cwd, env, initial, None)
    }

    /// Like [`Self::spawn`], but mirrors output to a durable on-disk log
    /// at `persist_path` so the terminal's scrollback survives a daemon
    /// restart. Any bytes a prior run left in that file seed the replay
    /// ahead of the fresh child's output — this is how a respawned
    /// session (whose child died with the old daemon) comes back with its
    /// history instead of a blank grid (#468). The file is bounded to
    /// [`SCROLLBACK_PERSIST_BYTES`]; keying it per session is the caller's
    /// job (see the spawn handler). Persistence is best-effort: an
    /// unwritable path degrades to an in-memory-only ring, never a failed
    /// spawn.
    pub fn spawn_persistent(
        cmd: &[String],
        size: PtySize,
        cwd: Option<&PathBuf>,
        env: Vec<(String, String)>,
        persist_path: PathBuf,
    ) -> Result<Self, PtyError> {
        let initial = read_scrollback_tail(&persist_path, SCROLLBACK_PERSIST_BYTES);
        let log = ScrollbackLog::open(persist_path);
        Self::spawn_inner(cmd, size, cwd, env, &initial, log)
    }

    fn spawn_inner(
        cmd: &[String],
        size: PtySize,
        cwd: Option<&PathBuf>,
        env: Vec<(String, String)>,
        initial: &[u8],
        mut persist: Option<ScrollbackLog>,
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
        command.env("COLORTERM", "truecolor");

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
        // The seed still counts as chunk seq 1 — the reader thread's
        // first live chunk is seq 2 and a subscribe snapshot replays the
        // seed ahead of live bytes — but it does NOT live in the ring:
        // as ring chunk 1 it consumed the eviction budget and vanished
        // once live churn wrapped the ring, taking every later
        // resync/snapshot's reattach scrollback with it (#420). The
        // durable slot keeps it out of eviction's reach and leaves the
        // full ring capacity to live output.
        let seed: Arc<[u8]> = Arc::from(initial);
        let seeded = !seed.is_empty();
        let ring = Arc::new(Mutex::new(ReplayRing::with_capacity(REPLAY_RING_BYTES)));
        let finished = Arc::new(AtomicBool::new(false));
        let finished_notify = Arc::new(Notify::new());
        let last_seq = Arc::new(AtomicU64::new(u64::from(seeded)));

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
                            if let Some(log) = persist.as_mut() {
                                log.append(&buf[..n]);
                            }
                            let bytes: Arc<[u8]> = Arc::from(&buf[..n]);
                            // Push to the ring and assign the seq under the
                            // SAME lock, in that order, so any reader of the
                            // (ring snapshot, last_seq) pair — `snapshot_only`,
                            // feeding subscribe + the forwarder's resync — sees
                            // the ring already containing every chunk through
                            // last_seq. Bumping the seq before the push left a
                            // window where last_seq led the ring by one chunk;
                            // the forwarder's seq dedup would then mistake that
                            // not-yet-replayed chunk for a duplicate and drop
                            // it. Ring-ahead-of-seq is harmless (worst case a
                            // duplicate that dedup catches); seq-ahead-of-ring
                            // loses data.
                            let seq = {
                                let mut r = reader_ring.blocking_lock();
                                r.push(&bytes);
                                reader_seq.fetch_add(1, Ordering::SeqCst) + 1
                            };
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

        // Exit watcher — blocking `wait` on another thread, publishes
        // the exit code on a watch channel so any number of awaiters
        // (and repeated calls) can observe it.
        let (exit_tx, exit_watch) = watch::channel::<Option<Option<i32>>>(None);
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
                let _ = exit_tx.send(Some(code));
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        Ok(Self {
            writer_tx,
            master: Arc::new(Mutex::new(pair.master)),
            output_tx,
            ring,
            finished,
            finished_notify,
            exit_watch,
            last_seq,
            seed,
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
    /// fire downstream. No-op (logged) if the pid wasn't captured at
    /// spawn — the terminal cannot be signalled at all in that case,
    /// and a silent return here left such sessions wedged with no
    /// breadcrumb.
    pub fn kill(&self) {
        // SIGTERM rather than SIGKILL so the agent gets a chance
        // to clean up its session file / save state.
        self.signal_child(SIGTERM_CODE);
    }

    /// SIGKILL the child — the escalation rung for a child that
    /// ignored `kill()`'s SIGTERM past the backend's grace period.
    pub fn force_kill(&self) {
        self.signal_child(SIGKILL_CODE);
    }

    fn signal_child(&self, sig: i32) {
        let Some(pid) = self.child_pid else {
            tracing::warn!(
                sig,
                "PTY kill: portable-pty reported no child pid at spawn — cannot signal; \
                 the child will only exit on its own"
            );
            return;
        };
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, sig);
        }
        #[cfg(not(unix))]
        {
            let _ = (pid, sig);
        }
    }

    /// Fire up a subscription: the current ring snapshot + a live feed.
    ///
    /// Subscribe to the broadcast BEFORE snapshotting the ring. The
    /// reader thread pushes to the ring then broadcasts (outside the ring
    /// lock), so snapshotting first leaves a window where a chunk is
    /// broadcast after our snapshot but before our receiver exists —
    /// lost from both replay and live. Subscribing first instead lets a
    /// chunk appear in both; `last_seq` is the replay high-water mark, so
    /// the consumer drops live chunks with `seq <= last_seq`.
    pub async fn subscribe(&self) -> Subscription {
        let live = self.output_tx.subscribe();
        let snapshot = self.snapshot_only().await;
        Subscription {
            replay: snapshot.replay,
            replay_complete: snapshot.complete,
            last_seq: snapshot.last_seq,
            live,
        }
    }

    /// Just the replay snapshot + last_seq, no new broadcast subscriber.
    /// Used by `Subscribe` snapshot path so reconnecting `--connect`
    /// clients can reconstruct their terminals without leaking a
    /// drainless broadcast receiver + pump task per snapshot call.
    ///
    /// The durable reattach seed (when present) is prepended ahead of
    /// the ring bytes on EVERY call — it sits outside the ring, so no
    /// amount of live churn evicts it and resyncs never shrink the
    /// reattach scrollback (#420). `complete` covers only the live
    /// stream: true while the ring still holds every byte the reader
    /// emitted since spawn. The seed is that stream's baseline, so
    /// seed + complete ring remains an authoritative VT reset.
    pub async fn snapshot_only(&self) -> crate::backend::ReplaySnapshot {
        let ring = self.ring.lock().await;
        let mut replay = Vec::with_capacity(self.seed.len() + ring.len());
        replay.extend_from_slice(&self.seed);
        ring.replay_snapshot_into(&mut replay);
        crate::backend::ReplaySnapshot {
            replay,
            last_seq: self.last_seq.load(Ordering::SeqCst),
            complete: ring.is_complete(),
        }
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
    /// unobservable (rare — `child.wait` error). Repeatable and safe to
    /// call concurrently: the code is published on a watch channel and
    /// retained, so every caller (and every repeat) sees the same value.
    pub async fn wait_exit(&self) -> Option<i32> {
        let mut rx = self.exit_watch.clone();
        loop {
            if let Some(code) = *rx.borrow_and_update() {
                return code;
            }
            if rx.changed().await.is_err() {
                // Watcher thread gone without publishing — unobservable.
                return None;
            }
        }
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
        assert!(r.is_complete());
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
        assert!(
            !r.is_complete(),
            "a byte tail cannot be used as an authoritative VT baseline"
        );
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

    /// The circular rewrite must survive many full revolutions: after
    /// every push the snapshot is exactly the last `cap` bytes of the
    /// concatenated input, regardless of where `head` currently sits.
    #[test]
    fn many_wraps_snapshot_matches_reference_tail() {
        let cap = 16;
        let mut r = ReplayRing::with_capacity(cap);
        let mut reference: Vec<u8> = Vec::new();
        // Chunk sizes chosen to hit every alignment: 1, 3, 7, 13 cycle
        // over a 16-byte ring so the wrap point lands everywhere.
        let sizes = [1usize, 3, 7, 13];
        let mut next: u8 = 0;
        for i in 0..50 {
            let n = sizes[i % sizes.len()];
            let chunk: Vec<u8> = (0..n)
                .map(|_| {
                    next = next.wrapping_add(1);
                    next
                })
                .collect();
            r.push(&chunk);
            reference.extend_from_slice(&chunk);
            let tail_start = reference.len().saturating_sub(cap);
            assert_eq!(
                r.snapshot(),
                &reference[tail_start..],
                "snapshot diverged after push #{i} (len {n})"
            );
            assert_eq!(r.total_written, reference.len() as u64);
            assert!(r.len() <= cap);
        }
    }

    /// A push that lands exactly on the wrap boundary (fills the ring to
    /// the byte) keeps head/state consistent for the next push.
    #[test]
    fn push_exactly_to_boundary_then_continue() {
        let mut r = ReplayRing::with_capacity(6);
        r.push(b"abcd");
        r.push(b"ef"); // exactly full, no overwrite yet
        assert_eq!(r.snapshot(), b"abcdef");
        r.push(b"gh"); // overwrites 'a','b'
        assert_eq!(r.snapshot(), b"cdefgh");
        assert_eq!(r.total_written, 8);
    }

    /// A burst exactly equal to capacity replaces the entire contents,
    /// even when the ring was mid-wrap (head != 0).
    #[test]
    fn cap_sized_burst_replaces_everything_when_wrapped() {
        let mut r = ReplayRing::with_capacity(4);
        r.push(b"abc");
        r.push(b"de"); // wrapped: holds "bcde", head != 0
        assert_eq!(r.snapshot(), b"bcde");
        r.push(b"WXYZ");
        assert_eq!(r.snapshot(), b"WXYZ");
        assert_eq!(r.total_written, 9);
    }

    /// One push that both finishes the fill phase and wraps: part of it
    /// tops the storage up to cap, the remainder overwrites the oldest
    /// bytes.
    #[test]
    fn single_push_spanning_fill_and_overwrite_phases() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abc");
        r.push(b"defg"); // "de" fills, "fg" overwrites "ab"
        assert_eq!(r.snapshot(), b"cdefg");
        assert_eq!(r.total_written, 7);
        assert_eq!(r.len(), 5);
    }

    /// A complete ring (nothing evicted) is a clean baseline as-is, so the
    /// replay snapshot keeps its first byte — trimming would wrongly drop
    /// the terminal's real opening line.
    #[test]
    fn replay_snapshot_keeps_head_while_complete() {
        let mut r = ReplayRing::with_capacity(32);
        r.push(b"line one\nline two\n");
        assert!(r.is_complete());
        assert_eq!(r.replay_snapshot(), b"line one\nline two\n");
    }

    /// A wrapped ring's oldest retained byte can be mid-line; the replay
    /// snapshot drops that partial leading line so a VT reconstruction
    /// starts on a clean boundary.
    #[test]
    fn replay_snapshot_drops_partial_leading_line_when_wrapped() {
        let mut r = ReplayRing::with_capacity(7);
        r.push(b"aaa\nbbb\nccc\n");
        // The last 7 bytes begin mid "bbb" line.
        assert_eq!(r.snapshot(), b"bb\nccc\n");
        // The replay snapshot drops that partial line, keeping only the
        // clean "ccc\n" that follows the first newline.
        assert_eq!(r.replay_snapshot(), b"ccc\n");
        assert!(!r.is_complete());
    }

    /// With no newline in the retained tail there is no safe boundary to
    /// cut on, so the replay snapshot returns the raw tail unchanged
    /// rather than discarding everything.
    #[test]
    fn replay_snapshot_keeps_tail_without_a_boundary() {
        let mut r = ReplayRing::with_capacity(4);
        r.push(b"abcdefgh");
        assert_eq!(r.snapshot(), b"efgh");
        assert_eq!(r.replay_snapshot(), b"efgh");
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    fn small() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// The spawned child sees `TERM=xterm-256color` AND
    /// `COLORTERM=truecolor`. `TERM` alone is not enough: modern TUIs
    /// (Codex among them) gate full color on `COLORTERM`, and without
    /// it they render degraded/monochrome. Regression for #421. The
    /// forced pair also wins over caller-provided values, matching the
    /// documented override semantics.
    #[tokio::test]
    async fn spawn_env_forces_term_and_colorterm() {
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf 'TERM=[%s] COLORTERM=[%s]' "$TERM" "$COLORTERM""#.to_string(),
            ],
            small(),
            None,
            vec![("COLORTERM".to_string(), "none".to_string())],
            &[],
        )
        .expect("spawn");
        pty.wait_finished().await;

        let sub = pty.subscribe().await;
        let out = String::from_utf8_lossy(&sub.replay).into_owned();
        assert!(
            out.contains("TERM=[xterm-256color]"),
            "TERM forced in child env: {out:?}"
        );
        assert!(
            out.contains("COLORTERM=[truecolor]"),
            "COLORTERM forced in child env: {out:?}"
        );
    }

    /// A seeded spawn replays the seed AHEAD of the child's own output:
    /// the seed occupies the front of the ring snapshot, live bytes
    /// follow, and `last_seq` accounts for the seed chunk so the
    /// consumer's seq-gap dedup never mistakes the first live chunk for
    /// a replayed duplicate.
    #[tokio::test]
    async fn seeded_spawn_replays_seed_before_live_output() {
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf live-bytes".to_string(),
            ],
            small(),
            None,
            Vec::new(),
            b"seeded-history\r\n",
        )
        .expect("spawn");
        pty.wait_finished().await;

        let sub = pty.subscribe().await;
        assert!(
            sub.replay.starts_with(b"seeded-history\r\n"),
            "seed must lead the replay: {:?}",
            String::from_utf8_lossy(&sub.replay)
        );
        let live_part = &sub.replay[b"seeded-history\r\n".len()..];
        assert!(
            String::from_utf8_lossy(live_part).contains("live-bytes"),
            "live output must follow the seed: {:?}",
            String::from_utf8_lossy(&sub.replay)
        );
        assert!(
            sub.last_seq >= 2,
            "seed is seq 1, live chunks continue from 2 (last_seq={})",
            sub.last_seq
        );
    }

    /// Regression for #420: the reattach seed must survive ring churn.
    /// It used to live only as ring chunk seq 1, so once live output
    /// wrapped the ring's byte budget the seed was evicted and every
    /// later resync/snapshot rebuilt clients WITHOUT their reattach
    /// scrollback ("scroll a bit, then it disappears"). A snapshot
    /// taken after the child pushed more than the ring's capacity must
    /// still replay the seed first.
    #[tokio::test]
    async fn seed_survives_ring_churn_past_capacity() {
        // ~2.6 MiB of live output (40k lines × 66 bytes after the PTY's
        // LF→CRLF translation) — comfortably past REPLAY_RING_BYTES.
        let line = "x".repeat(64);
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("yes {line} | head -n 40000; printf churn-tail-marker"),
            ],
            small(),
            None,
            Vec::new(),
            b"seeded-history\r\n",
        )
        .expect("spawn");
        pty.wait_finished().await;

        let snap = pty.snapshot_only().await;
        assert!(
            snap.replay.starts_with(b"seeded-history\r\n"),
            "the reattach seed must survive live churn past the ring capacity"
        );
        assert!(
            String::from_utf8_lossy(&snap.replay).contains("churn-tail-marker"),
            "the retained ring tail must still follow the seed"
        );
        assert!(
            !snap.complete,
            "live bytes were evicted — the snapshot must not claim a complete live stream"
        );
    }

    /// Regression for #420 (companion): a large seed must not consume
    /// the ring's live-byte budget. When the seed was ring chunk 1, a
    /// capture near the ring capacity left almost no room for live
    /// output, so the first flurry of bytes evicted the seed and
    /// flipped every snapshot incomplete — resyncs went unavailable
    /// moments after reattach. With the durable slot the whole ring
    /// budget belongs to live output: the snapshot stays complete and
    /// the seed intact.
    #[tokio::test]
    async fn large_seed_leaves_full_ring_budget_for_live_output() {
        let seed = vec![b'H'; REPLAY_RING_BYTES - 1024];
        // ~6.6 KiB of live output — trivial for a 2 MiB ring, but more
        // than the 1 KiB the seed used to leave of it, so this fails if
        // the seed ever counts against the ring budget again.
        let line = "x".repeat(64);
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("yes {line} | head -n 100; printf live-bytes"),
            ],
            small(),
            None,
            Vec::new(),
            &seed,
        )
        .expect("spawn");
        pty.wait_finished().await;

        let snap = pty.snapshot_only().await;
        assert!(
            snap.complete,
            "live output far under the ring capacity must stay complete \
             regardless of seed size"
        );
        assert!(
            snap.replay.starts_with(&seed[..]),
            "the full seed must lead the snapshot"
        );
        assert!(
            String::from_utf8_lossy(&snap.replay).contains("live-bytes"),
            "live output must follow the seed"
        );
    }

    /// Regression for #420's follow-up comment: the durable seed must
    /// hold while the agent sits on the ALTERNATE screen (full-screen
    /// TUIs — Claude/Codex). The reported collapse — history paints
    /// briefly on reattach, then drops to ~2 lines the moment the
    /// alt-screen VT reasserts — combined the volatile seed with an alt
    /// screen that owns no primary scrollback. The daemon half fixed
    /// here is screen-agnostic and must stay that way: with the child
    /// parked on the alt screen, every snapshot still replays the seed
    /// first and stays complete (resync-servable), so no interaction
    /// can rebuild a client without its reattach history. (Pane-level
    /// alt-screen denial is #393 / PR #427; this pins that no screen
    /// mode bypasses the durable slot.)
    #[tokio::test]
    async fn seed_survives_while_child_holds_the_alt_screen() {
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                // Enter the alternate screen, repaint like a TUI, and
                // exit while still holding it — the shape reattach sees
                // when a full-screen agent owns the pane.
                "printf '\\033[?1049h'; yes 'painting the alt screen' | head -n 200; \
                 printf alt-screen-tail"
                    .to_string(),
            ],
            small(),
            None,
            Vec::new(),
            b"seeded-history\r\n",
        )
        .expect("spawn");
        pty.wait_finished().await;

        let snap = pty.snapshot_only().await;
        assert!(
            snap.replay.starts_with(b"seeded-history\r\n"),
            "the seed must lead the snapshot even with the child on the alt screen"
        );
        let after_seed = &snap.replay[b"seeded-history\r\n".len()..];
        assert!(
            after_seed
                .windows(b"\x1b[?1049h".len())
                .any(|w| w == b"\x1b[?1049h"),
            "the alt-screen switch must replay AFTER the seed, so a rebuilt \
             client keeps the seed as its scrollback baseline"
        );
        assert!(
            String::from_utf8_lossy(&snap.replay).contains("alt-screen-tail"),
            "live alt-screen output must follow the seed"
        );
        assert!(
            snap.complete,
            "modest alt-screen churn must leave the snapshot resync-servable"
        );
    }

    /// An empty seed changes nothing: seq numbering starts at 0 and the
    /// replay holds only what the child wrote.
    #[tokio::test]
    async fn unseeded_spawn_is_unchanged() {
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf only-live".to_string(),
            ],
            small(),
            None,
            Vec::new(),
            &[],
        )
        .expect("spawn");
        pty.wait_finished().await;

        let sub = pty.subscribe().await;
        assert!(
            String::from_utf8_lossy(&sub.replay).contains("only-live"),
            "replay carries the child's output"
        );
        assert!(
            !String::from_utf8_lossy(&sub.replay).contains("seeded"),
            "nothing but child output in the ring"
        );
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    fn small() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    /// The whole point of #468: after the process that produced the
    /// output is gone, a FRESH PTY seeded from the same persist path
    /// replays that output — the durable file, not the dead ring, carries
    /// the history across the (simulated) daemon restart.
    #[tokio::test]
    async fn persisted_output_seeds_a_later_spawn() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("session-a.log");

        let first = DaemonPty::spawn_persistent(
            &sh("printf 'history-line\\n'"),
            small(),
            None,
            Vec::new(),
            path.clone(),
        )
        .expect("spawn");
        first.wait_finished().await;
        // Drop the first PTY entirely — its ring is gone, only the file
        // remains, exactly as after a daemon restart.
        drop(first);

        let second = DaemonPty::spawn_persistent(
            &sh("printf 'fresh-output'"),
            small(),
            None,
            Vec::new(),
            path.clone(),
        )
        .expect("respawn");
        second.wait_finished().await;

        let replay = String::from_utf8_lossy(&second.snapshot_only().await.replay).into_owned();
        assert!(
            replay.contains("history-line"),
            "the respawn must replay the prior process's persisted history: {replay:?}"
        );
        assert!(
            replay.contains("fresh-output"),
            "and its own fresh output after it: {replay:?}"
        );
        let history_at = replay.find("history-line").unwrap();
        let fresh_at = replay.find("fresh-output").unwrap();
        assert!(history_at < fresh_at, "history seeds ahead of live output");
    }

    /// An unpersisted spawn (plain `spawn`) writes no file, and a persist
    /// path that never existed seeds an empty replay — a fresh session
    /// starts clean rather than erroring on the missing file.
    #[tokio::test]
    async fn missing_persist_file_seeds_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("brand-new.log");
        assert!(!path.exists());

        let pty = DaemonPty::spawn_persistent(
            &sh("printf only-live"),
            small(),
            None,
            Vec::new(),
            path.clone(),
        )
        .expect("spawn");
        pty.wait_finished().await;

        assert!(path.exists(), "the log is created as output flows");
        let replay = String::from_utf8_lossy(&pty.snapshot_only().await.replay).into_owned();
        assert!(replay.contains("only-live"));
        assert!(
            !replay.contains("brand-new"),
            "nothing seeded from a previously-empty session"
        );
    }

    /// The on-disk file is bounded: a burst far past the retain size
    /// compacts down to (roughly) the last `SCROLLBACK_PERSIST_BYTES`,
    /// never growing without limit, while still carrying the recent tail
    /// forward to a reseed.
    #[test]
    fn scrollback_log_is_bounded_and_keeps_the_tail() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("busy.log");
        let mut log = ScrollbackLog::open(path.clone()).expect("open");

        // Write comfortably past the compaction trigger in line-sized
        // chunks so the trimmed head still lands on a boundary.
        let line = {
            let mut l = vec![b'x'; 4096];
            l.push(b'\n');
            l
        };
        let mut total = 0u64;
        while total <= SCROLLBACK_COMPACT_BYTES + line.len() as u64 {
            log.append(&line);
            total += line.len() as u64;
        }
        // A final unique marker so we can prove the tail survived.
        log.append(b"TAIL-MARKER\n");
        drop(log);

        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert!(
            on_disk <= SCROLLBACK_COMPACT_BYTES,
            "file compacts under the bound: {on_disk} > {SCROLLBACK_COMPACT_BYTES}"
        );
        let seed = read_scrollback_tail(&path, SCROLLBACK_PERSIST_BYTES);
        assert!(
            seed.windows(b"TAIL-MARKER".len())
                .any(|w| w == b"TAIL-MARKER"),
            "the most recent output survives compaction"
        );
    }

    /// A cut-in-the-middle tail is trimmed to the next line boundary so a
    /// reseed never begins inside a partial escape/UTF-8 sequence — the
    /// same clean-baseline property tmux's line-oriented capture has.
    #[test]
    fn tail_trims_to_a_line_boundary_when_truncated() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lines.log");
        std::fs::write(&path, b"aaaa\nbbbb\ncccc\n").unwrap();
        // Ask for fewer bytes than the file holds: the read starts mid
        // "aaaa" and must drop that partial first line.
        let tail = read_scrollback_tail(&path, 9);
        assert_eq!(
            tail, b"cccc\n",
            "partial leading line dropped, got {tail:?}"
        );
    }
}

#[cfg(test)]
mod exit_tests {
    use super::*;

    fn small() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[tokio::test]
    async fn wait_exit_returns_cached_code_on_repeat() {
        let pty = DaemonPty::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 7".to_string(),
            ],
            small(),
            None,
            Vec::new(),
            &[],
        )
        .expect("spawn");

        let first = pty.wait_exit().await;
        assert_eq!(first, Some(7));
        // The tmux backend calls this more than once; a oneshot used to
        // make every call after the first return None.
        assert_eq!(pty.wait_exit().await, first);
        assert_eq!(pty.wait_exit().await, first);
    }

    #[tokio::test]
    async fn concurrent_wait_exit_all_observe_the_code() {
        let pty = std::sync::Arc::new(
            DaemonPty::spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "exit 3".to_string(),
                ],
                small(),
                None,
                Vec::new(),
                &[],
            )
            .expect("spawn"),
        );

        let a = {
            let p = pty.clone();
            tokio::spawn(async move { p.wait_exit().await })
        };
        let b = {
            let p = pty.clone();
            tokio::spawn(async move { p.wait_exit().await })
        };
        assert_eq!(a.await.unwrap(), Some(3));
        assert_eq!(b.await.unwrap(), Some(3));
    }
}
