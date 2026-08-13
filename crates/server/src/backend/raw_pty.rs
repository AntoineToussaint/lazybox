//! `SessionBackend` impl that opens a PTY directly via `portable-pty`.
//!
//! This is the historical default — lazybox used to embed a PTY inside
//! the server with no external session manager. It still works, but
//! it's now one [`SessionBackend`] among others. Sessions die when
//! the server process exits; `list()` always returns empty because
//! there's no shared state to reconnect to.
//!
//! Use this backend for `--test` mode, in environments without
//! `tmux`, or in unit tests that don't want subprocess ceremony.

use crate::backend::{BackendError, OutputChunk, SessionBackend, Subscription};
use crate::pty::DaemonPty;
use portable_pty::PtySize;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, watch};

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// How long `kill()` waits for a SIGTERM'd child to exit before
/// escalating to SIGKILL. A child that traps/ignores SIGTERM would
/// otherwise never EOF, `TerminalExited` would never fire, and the
/// terminal wedged forever with a second Close a no-op.
const KILL_ESCALATION_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Internal map: backend session key → live PTY + cached exit code.
struct Slot {
    pty: Arc<DaemonPty>,
    /// Once the child exits, the spawn-side watcher task publishes
    /// the exit code here. A `watch` channel — same mechanism as
    /// `DaemonPty::exit_watch` (see `pty.rs` and
    /// docs/codebase-review-236.md) — so any number of concurrent
    /// `wait_exit` callers await the change instead of polling, and
    /// repeated calls read the retained code immediately. `None`
    /// while the child is alive.
    exit: watch::Receiver<Option<Option<i32>>>,
}

pub struct RawPtyBackend {
    /// `Arc` so the kill-escalation task can remove the slot once the
    /// child's exit is actually observed (the slot must survive until
    /// then so a second `kill` can retry a stuck child).
    sessions: Arc<Mutex<HashMap<String, Slot>>>,
    next_key: AtomicU64,
    /// SIGTERM → SIGKILL escalation window. [`KILL_ESCALATION_GRACE`]
    /// in production; tests shrink it so a trap-TERM child escalates
    /// within the test deadline.
    kill_grace: std::time::Duration,
}

impl RawPtyBackend {
    pub fn new() -> Self {
        Self::with_kill_grace(KILL_ESCALATION_GRACE)
    }

    /// `new()` with an explicit SIGTERM→SIGKILL grace period. Test
    /// hook — production callers use `new()`.
    pub fn with_kill_grace(kill_grace: std::time::Duration) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_key: AtomicU64::new(1),
            kill_grace,
        }
    }

    /// Shared body for [`SessionBackend::spawn`] and
    /// [`SessionBackend::spawn_persistent`]. `persist` selects an on-disk
    /// scrollback file when the caller wants restart-durable history.
    fn spawn_inner<'a>(
        &'a self,
        argv: &'a [String],
        cwd: Option<&'a Path>,
        env: &'a [(String, String)],
        hint: &'a str,
        persist: Option<std::path::PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<String, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let size = PtySize {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                pixel_width: 0,
                pixel_height: 0,
            };
            let cwd = cwd.map(|p| p.to_path_buf());
            let pty = match persist {
                Some(path) => {
                    DaemonPty::spawn_persistent(argv, size, cwd.as_ref(), env.to_vec(), path)
                }
                None => DaemonPty::spawn(argv, size, cwd.as_ref(), env.to_vec(), &[]),
            }
            .map_err(|e| BackendError::Spawn(e.to_string()))?;
            let pty = Arc::new(pty);
            let key = self.alloc_key(hint);
            let (exit_tx, exit_rx) = watch::channel::<Option<Option<i32>>>(None);
            let slot = Slot {
                pty: pty.clone(),
                exit: exit_rx,
            };
            // Background task that watches for exit and publishes the
            // code on the watch channel. The watch retains the value,
            // so every `wait_exit` call — concurrent or repeated —
            // gets the cached code (the trait contract).
            let pty_for_exit = pty.clone();
            tokio::spawn(async move {
                let code = pty_for_exit.wait_exit().await;
                let _ = exit_tx.send(Some(code));
            });
            self.sessions.lock().await.insert(key.clone(), slot);
            Ok(key)
        })
    }

    fn alloc_key(&self, hint: &str) -> String {
        let n = self.next_key.fetch_add(1, Ordering::Relaxed);
        // Sanitize the hint the same way the tmux backend does so
        // logs reading both backends look consistent.
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
        format!("raw-{safe}-{n}")
    }
}

impl Default for RawPtyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionBackend for RawPtyBackend {
    fn id(&self) -> &'static str {
        "raw-pty"
    }

    fn spawn<'a>(
        &'a self,
        argv: &'a [String],
        cwd: Option<&'a Path>,
        env: &'a [(String, String)],
        hint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, BackendError>> + Send + 'a>> {
        self.spawn_inner(argv, cwd, env, hint, None)
    }

    /// Raw-PTY children die with the daemon, so a respawn after a restart
    /// starts with an empty grid. When a persist key is supplied, seed
    /// the fresh PTY from — and keep mirroring output to — the session's
    /// durable scrollback file, so its history survives the restart
    /// (#468).
    fn spawn_persistent<'a>(
        &'a self,
        argv: &'a [String],
        cwd: Option<&'a Path>,
        env: &'a [(String, String)],
        hint: &'a str,
        persist_key: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String, BackendError>> + Send + 'a>> {
        let persist = persist_key.map(|k| lazybox_core::paths::scrollback_dir().join(k));
        self.spawn_inner(argv, cwd, env, hint, persist)
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
                    .map(|s| s.pty.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            pty.write(bytes)
                .await
                .map_err(|e| BackendError::Other(e.to_string()))
        })
    }

    fn output_seq<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<u64, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.pty.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            Ok(pty.output_seq())
        })
    }

    fn resize<'a>(
        &'a self,
        key: &'a str,
        cols: u16,
        rows: u16,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.pty.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            pty.resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .await
            .map_err(|e| BackendError::Other(e.to_string()))
        })
    }

    fn kill<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            // Look the slot up WITHOUT removing it: the slot only
            // leaves the map once the child's exit is actually
            // observed. Removing first (the old behavior) made a
            // SIGTERM-ignoring child unkillable — the single SIGTERM
            // was the only shot, and a second Close found no slot and
            // no-op'd while the terminal wedged forever.
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key).map(|s| s.pty.clone())
            };
            let Some(pty) = pty else {
                return Ok(()); // already gone — kill is idempotent
            };
            if pty.is_finished() {
                // Exit already observed (self-exited child whose slot
                // hasn't been released yet) — just drop the slot; no
                // signal, the pid may already be recycled.
                self.sessions.lock().await.remove(key);
                return Ok(());
            }
            // SIGTERM first so the agent can save state; the output
            // pump observes the broadcast close and emits
            // TerminalExited upstream.
            pty.kill();
            // Escalation + slot release. SIGKILL after the grace
            // period if the child ignored SIGTERM; the slot is removed
            // only once the exit is observed, and stays behind as a
            // retryable tombstone if even SIGKILL doesn't take
            // (unkillable D-state child).
            let sessions = self.sessions.clone();
            let grace = self.kill_grace;
            let key = key.to_string();
            tokio::spawn(async move {
                if tokio::time::timeout(grace, pty.wait_exit()).await.is_err() {
                    tracing::warn!(
                        key = %key,
                        grace_ms = grace.as_millis() as u64,
                        "child ignored SIGTERM past the grace period — escalating to SIGKILL"
                    );
                    pty.force_kill();
                }
                if tokio::time::timeout(grace, pty.wait_exit()).await.is_ok() {
                    sessions.lock().await.remove(&key);
                } else {
                    tracing::warn!(
                        key = %key,
                        "child survived SIGKILL (unkillable?) — keeping the slot so kill can be retried"
                    );
                }
            });
            Ok(())
        })
    }

    fn release<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // Called by the pump teardown after the session's exit was
            // observed. Dropping the slot drops the last long-lived
            // `Arc<DaemonPty>` reference once the subscribe bridges
            // unwind, which closes the master fd and ends the writer
            // thread (its queue sender is dropped with the DaemonPty).
            self.sessions.lock().await.remove(key);
        })
    }

    fn list<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let map = self.sessions.lock().await;
            Ok(map.keys().cloned().collect())
        })
    }

    fn is_alive<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let map = self.sessions.lock().await;
            Ok(map.get(key).is_some_and(|slot| !slot.pty.is_finished()))
        })
    }

    fn snapshot<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<
        Box<dyn Future<Output = Result<crate::backend::ReplaySnapshot, BackendError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.pty.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            Ok(pty.snapshot_only().await)
        })
    }

    fn read_since<'a>(
        &'a self,
        key: &'a str,
        since: u64,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<crate::backend::OutputDelta>, BackendError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.pty.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            // `None` (ring evicted the watermark) propagates as a
            // no-gap-free-delta signal — the caller falls back to a full
            // snapshot, same as the tmux opt-out.
            Ok(pty.read_since(since).await)
        })
    }

    fn subscribe<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Subscription, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let pty = {
                let map = self.sessions.lock().await;
                map.get(key)
                    .map(|s| s.pty.clone())
                    .ok_or_else(|| BackendError::NotFound(key.into()))?
            };
            // DaemonPty emits a broadcast::Receiver; bridge to mpsc so
            // the trait stays simple. A spawned task drains the
            // broadcast and forwards as OutputChunks; on Closed or
            // child finished it just drops the sender (closes mpsc).
            // Bounded: a stalled subscriber drops chunks (`try_send`)
            // instead of buffering without limit — the spawn pump's
            // seq-gap detection (`spawn_handler`) sees the hole on the
            // next delivered chunk and resyncs from the replay ring.
            let mut sub = pty.subscribe().await;
            let replay = std::mem::take(&mut sub.replay);
            let replay_complete = sub.replay_complete;
            let last_seq = sub.last_seq;
            let (tx, rx) = tokio::sync::mpsc::channel::<OutputChunk>(
                crate::backend::SUBSCRIPTION_CHANNEL_CAPACITY,
            );

            let pty_for_pump = pty.clone();
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
                                        tracing::debug!(
                                            key = %bridge_key,
                                            seq = c.seq,
                                            "raw-pty bridge channel full — dropping chunk"
                                        );
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        return; // subscriber dropped
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::info!(
                                    key = %bridge_key,
                                    missed = n,
                                    "raw-pty bridge lagged behind PTY broadcast — chunks skipped"
                                );
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                        _ = pty_for_pump.wait_finished() => break,
                    }
                }
                // tx drops here → mpsc closes → subscriber sees None.
            });
            Ok(Subscription {
                replay,
                replay_complete,
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
            let mut exit = {
                let map = self.sessions.lock().await;
                map.get(key).map(|s| s.exit.clone())?
            };
            // Await the published exit code — no polling. The watch
            // retains the value, so a call made after the exit
            // returns the cached code immediately.
            loop {
                if let Some(code) = *exit.borrow_and_update() {
                    return code;
                }
                if exit.changed().await.is_err() {
                    // Publisher task gone without sending — the exit
                    // was unobservable.
                    return None;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    /// Poll until the backend's session map drains or `deadline` runs
    /// out. Slot removal happens on background tasks, so the tests
    /// converge on it rather than asserting immediately.
    async fn sessions_drained(b: &RawPtyBackend, deadline: Duration) -> bool {
        let end = tokio::time::Instant::now() + deadline;
        while tokio::time::Instant::now() < end {
            if b.sessions.lock().await.is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// Regression (slot leak on self-exit): a child that exits on its
    /// own keeps its slot only until `release` — which the daemon's
    /// pump teardown calls after observing the exit — drops it. Before
    /// the fix nothing but `kill()` ever removed slots, so the open
    /// PTY master, parked writer thread and replay ring lived forever.
    #[tokio::test]
    async fn release_drops_slot_after_self_exit() {
        timeout(Duration::from_secs(10), async {
            let b = RawPtyBackend::new();
            let key = b.spawn(&sh("exit 0"), None, &[], "t").await.expect("spawn");
            assert_eq!(b.wait_exit(&key).await, Some(0));
            assert_eq!(
                b.list().await.expect("list"),
                vec![key.clone()],
                "slot survives until the pump teardown releases it"
            );
            b.release(&key).await;
            assert!(
                b.list().await.expect("list").is_empty(),
                "release drops the slot"
            );
            // Idempotent — releasing again is a no-op.
            b.release(&key).await;
        })
        .await
        .expect("test deadline exceeded");
    }

    /// Regression (no SIGKILL escalation): a child that traps SIGTERM
    /// used to wedge forever — kill() removed the slot first and sent
    /// one SIGTERM, so the terminal never EOF'd and a second Close
    /// no-op'd. Now kill() escalates to SIGKILL after the grace period
    /// and the slot is removed once the exit is observed.
    #[tokio::test]
    async fn kill_escalates_to_sigkill_for_term_trapping_child() {
        timeout(Duration::from_secs(10), async {
            let b = RawPtyBackend::with_kill_grace(Duration::from_millis(200));
            let key = b
                .spawn(
                    // Trap (ignore) SIGTERM; short inner sleeps so the
                    // shell's own children die quickly after SIGKILL.
                    &sh("trap '' TERM; while :; do sleep 0.1; done"),
                    None,
                    &[],
                    "t",
                )
                .await
                .expect("spawn");
            b.kill(&key).await.expect("kill");
            // Immediately after kill the slot must still exist (it only
            // leaves the map when the exit is observed) so a retry has
            // something to act on.
            assert!(
                !b.sessions.lock().await.is_empty(),
                "slot is not removed before the exit is observed"
            );
            assert!(
                sessions_drained(&b, Duration::from_secs(8)).await,
                "SIGKILL escalation reaps the trap-TERM child and releases the slot"
            );
        })
        .await
        .expect("test deadline exceeded");
    }

    /// Regression (unbounded 50ms poll loop): `wait_exit` used to spin
    /// on a mutexed slot every 50ms forever. It now awaits the exit
    /// code on a `watch` channel (the `DaemonPty` pattern) — the trait
    /// contract stays: two concurrent callers both observe the code,
    /// and a call made after the exit gets the cached code immediately.
    #[tokio::test]
    async fn wait_exit_serves_concurrent_and_repeated_callers() {
        timeout(Duration::from_secs(10), async {
            let b = RawPtyBackend::new();
            let key = b.spawn(&sh("exit 7"), None, &[], "t").await.expect("spawn");
            let (a, c) = tokio::join!(b.wait_exit(&key), b.wait_exit(&key));
            assert_eq!(a, Some(7), "first concurrent caller sees the code");
            assert_eq!(c, Some(7), "second concurrent caller sees the code");
            // After the exit the watch retains the value: a fresh call
            // returns the cached code.
            assert_eq!(b.wait_exit(&key).await, Some(7), "post-exit call is cached");
        })
        .await
        .expect("test deadline exceeded");
    }

    /// kill on an unknown key stays an Ok no-op (idempotence contract).
    #[tokio::test]
    async fn kill_unknown_key_is_ok() {
        let b = RawPtyBackend::new();
        b.kill("raw-nope-1").await.expect("idempotent");
    }
}
