//! In-memory `SessionBackend` for tests.
//!
//! Drop-in replacement for `RawPtyBackend` / `TmuxBackend` that does
//! NOT spawn any real subprocess. Tests inject synthetic output via
//! [`MockBackend::emit`] and trigger exit via [`MockBackend::finish`].
//!
//! Why this exists: the daemon's spawn pipeline (handle_spawn, the
//! per-PTY pump task, snapshot replay, kill-on-out-of-scope, …) is
//! the unit we care about. With real PTYs in tests, 13 `sleep 5`
//! shells running in parallel exhausted FD/PTY budgets and deadlocked
//! the workspace test run. Mocking the backend keeps every test in
//! milliseconds and removes the platform dependency on `sh`, `tmux`,
//! `curl`, etc.
//!
//! Coverage: this fake honors the same `SessionBackend` contract the
//! real backends do. Tests using it exercise the daemon end-to-end.

use crate::backend::{BackendError, OutputChunk, SessionBackend, Subscription};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{Mutex, mpsc, oneshot};

/// In-memory backend. Cheap to clone (it's just an `Arc<Mutex<...>>`).
#[derive(Default, Clone)]
pub struct MockBackend {
    inner: Arc<MockInner>,
}

#[derive(Default)]
struct MockInner {
    sessions: Mutex<HashMap<String, MockSession>>,
    counter: AtomicU64,
    /// Keys whose `snapshot()` should hang forever. Used by tests
    /// asserting the daemon's snapshot-timeout safety net works.
    wedged_snapshot_keys: Mutex<std::collections::HashSet<String>>,
    /// Per-key count of upcoming `snapshot()` calls that should return
    /// an error. Lets retry-contract tests fail once without sleeping
    /// through the timeout path.
    snapshot_failures: Mutex<HashMap<String, usize>>,
    /// Fault/delay injection for terminal command-lane isolation tests.
    wedged_write_keys: Mutex<std::collections::HashSet<String>>,
    wedged_resize_keys: Mutex<std::collections::HashSet<String>>,
    write_delays: Mutex<HashMap<String, std::time::Duration>>,
    write_attempts: Mutex<Vec<String>>,
    active_writes: AtomicUsize,
    max_concurrent_writes: AtomicUsize,
    /// Artificial delay applied at the start of every `spawn()`. Lets
    /// tests hold a spawn "mid-provision" to exercise the in-flight
    /// spawn races (duplicate collapse, Kill serialization).
    spawn_delay: Mutex<Option<std::time::Duration>>,
    /// Per-key kill failure injected by lifecycle tests. Real backends keep
    /// their slot on a transport/timeout failure so callers can retry; the
    /// mock must be able to exercise that contract too.
    kill_errors: Mutex<HashMap<String, String>>,
    /// Keys `release()` was called for, in call order. The mock keeps
    /// the session data around for post-mortem assertions (writes,
    /// argv), so release only records — tests use `released_keys()` to
    /// assert the pump teardown freed the backend slot.
    released: Mutex<Vec<String>>,
    /// Canned `scrollback()` payloads per key — the mock's stand-in
    /// for tmux's capture-pane history. Keys without an entry return
    /// `None`, matching a backend with no history source.
    deep_scrollback: Mutex<HashMap<String, Vec<u8>>>,
}

struct MockSession {
    /// Arguments captured for assertions.
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<std::path::PathBuf>,
    /// Bytes the daemon wrote to this session.
    writes: Vec<Vec<u8>>,
    /// Resize calls captured: (cols, rows).
    resizes: Vec<(u16, u16)>,
    /// Replay buffer — everything emitted so far. New subscribers get
    /// this in their `Subscription.replay`.
    replay: Vec<u8>,
    /// Monotonic chunk counter — matches real backend semantics.
    last_seq: u64,
    /// Fan-out to live subscribers. Each `subscribe()` registers one
    /// bounded `mpsc::Sender`; `emit()` try-sends to all of them
    /// (drop-on-full, matching the real backends' bridge semantics).
    subscribers: Vec<mpsc::Sender<OutputChunk>>,
    /// Exit code once `finish()` (or `kill()`) fires. None until then.
    exit_code: Option<i32>,
    /// Waiters parked in `wait_exit()`.
    exit_waiters: Vec<oneshot::Sender<Option<i32>>>,
}

struct ActiveWriteCall {
    inner: Arc<MockInner>,
}

impl Drop for ActiveWriteCall {
    fn drop(&mut self) {
        self.inner.active_writes.fetch_sub(1, Ordering::SeqCst);
    }
}

impl MockBackend {
    /// Construct a fresh, empty backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// As an `Arc<dyn SessionBackend>` — drop-in for the daemon.
    pub fn as_backend(&self) -> Arc<dyn SessionBackend> {
        Arc::new(self.clone())
    }

    /// Inject synthetic output. Each call becomes one chunk delivered
    /// to every active subscriber AND appended to the replay buffer.
    pub async fn emit(&self, key: &str, bytes: impl AsRef<[u8]>) {
        let bytes = bytes.as_ref().to_vec();
        let mut map = self.inner.sessions.lock().await;
        let Some(session) = map.get_mut(key) else {
            return;
        };
        session.last_seq += 1;
        let chunk = OutputChunk {
            seq: session.last_seq,
            bytes: bytes.clone(),
        };
        session.replay.extend_from_slice(&bytes);
        // Drop disconnected subscribers as we go; a full (stalled)
        // subscriber keeps its slot but misses this chunk — same
        // drop-on-overflow contract as the real backends.
        session.subscribers.retain(|tx| {
            !matches!(
                tx.try_send(chunk.clone()),
                Err(mpsc::error::TrySendError::Closed(_))
            )
        });
    }

    /// Mark the session exited with `code`. Subsequent `wait_exit`
    /// calls resolve to `Some(code)`. Mirrors a child-exit event.
    pub async fn finish(&self, key: &str, code: i32) {
        let mut map = self.inner.sessions.lock().await;
        let Some(session) = map.get_mut(key) else {
            return;
        };
        session.exit_code = Some(code);
        // Close subscribers so the pump task sees the live channel
        // close — same as a real session ending.
        session.subscribers.clear();
        for waiter in std::mem::take(&mut session.exit_waiters) {
            let _ = waiter.send(Some(code));
        }
    }

    /// Bytes the daemon wrote to this session, oldest first. Tests
    /// use this to assert the prompt-injection path delivered the
    /// expected bytes, etc.
    pub async fn writes_for(&self, key: &str) -> Vec<Vec<u8>> {
        let map = self.inner.sessions.lock().await;
        map.get(key).map(|s| s.writes.clone()).unwrap_or_default()
    }

    /// Backend keys whose write future was entered, including calls that are
    /// currently delayed or wedged before recording bytes.
    pub async fn write_attempts(&self) -> Vec<String> {
        self.inner.write_attempts.lock().await.clone()
    }

    /// Highest number of backend write futures active at once. The daemon's
    /// global terminal interaction contract should keep this at one even when
    /// multiple clients target the same terminal.
    pub fn max_concurrent_writes(&self) -> usize {
        self.inner.max_concurrent_writes.load(Ordering::SeqCst)
    }

    /// Resize calls captured for this session, oldest first.
    pub async fn resizes_for(&self, key: &str) -> Vec<(u16, u16)> {
        let map = self.inner.sessions.lock().await;
        map.get(key).map(|s| s.resizes.clone()).unwrap_or_default()
    }

    /// argv passed to `spawn()` for this session.
    pub async fn argv_for(&self, key: &str) -> Option<Vec<String>> {
        let map = self.inner.sessions.lock().await;
        map.get(key).map(|s| s.argv.clone())
    }

    /// Every argv passed to `spawn()` so far, in no particular order.
    /// For tests that drive a spawn through several layers and can't
    /// easily predict the generated backend session key.
    pub async fn all_argv(&self) -> Vec<Vec<String>> {
        let map = self.inner.sessions.lock().await;
        map.values().map(|s| s.argv.clone()).collect()
    }

    /// Environment passed to `spawn()` for this session.
    pub async fn env_for(&self, key: &str) -> Option<Vec<(String, String)>> {
        let map = self.inner.sessions.lock().await;
        map.get(key).map(|s| s.env.clone())
    }

    /// CWD passed to `spawn()` for this session.
    pub async fn cwd_for(&self, key: &str) -> Option<std::path::PathBuf> {
        let map = self.inner.sessions.lock().await;
        map.get(key).and_then(|s| s.cwd.clone())
    }

    /// Make every subsequent `spawn()` sleep for `delay` before
    /// registering the session — a stand-in for slow worktree
    /// provisioning / agent boot in race tests.
    pub async fn set_spawn_delay(&self, delay: std::time::Duration) {
        *self.inner.spawn_delay.lock().await = Some(delay);
    }

    /// Make `kill(key)` fail without closing or removing the session.
    pub async fn fail_kill(&self, key: &str, message: impl Into<String>) {
        self.inner
            .kill_errors
            .lock()
            .await
            .insert(key.to_string(), message.into());
    }

    /// Make `snapshot(key)` hang forever — used to test that the
    /// daemon's per-session snapshot timeout keeps the IPC channel
    /// flowing even when a backend gets stuck.
    pub async fn wedge_snapshot(&self, key: &str) {
        self.inner
            .wedged_snapshot_keys
            .lock()
            .await
            .insert(key.into());
    }

    /// Fail the next `count` snapshots for `key`, then resume normally.
    pub async fn fail_next_snapshots(&self, key: &str, count: usize) {
        self.inner
            .snapshot_failures
            .lock()
            .await
            .insert(key.into(), count);
    }

    /// Make writes for `key` never resolve. The server-side deadline and
    /// per-terminal worker isolation must keep other terminals responsive.
    pub async fn wedge_write(&self, key: &str) {
        self.inner.wedged_write_keys.lock().await.insert(key.into());
    }

    /// Make resizes for `key` never resolve.
    pub async fn wedge_resize(&self, key: &str) {
        self.inner
            .wedged_resize_keys
            .lock()
            .await
            .insert(key.into());
    }

    /// Delay writes for one key so tests can deterministically accumulate a
    /// burst and assert the command lane coalesces it without reordering.
    pub async fn set_write_delay(&self, key: &str, delay: std::time::Duration) {
        self.inner
            .write_delays
            .lock()
            .await
            .insert(key.into(), delay);
    }

    /// Simulate the real backends' bounded-bridge drop: the chunk lands
    /// in the replay ring (its `seq` is consumed) but is NOT delivered
    /// to live subscribers — exactly what a full mpsc bridge or a
    /// lagged broadcast produces. Lets tests exercise the pump's
    /// seq-gap resync deterministically.
    pub async fn emit_dropped(&self, key: &str, bytes: impl AsRef<[u8]>) {
        let mut map = self.inner.sessions.lock().await;
        let Some(session) = map.get_mut(key) else {
            return;
        };
        session.last_seq += 1;
        session.replay.extend_from_slice(bytes.as_ref());
    }

    /// Keys `release()` was called for so far, in call order.
    pub async fn released_keys(&self) -> Vec<String> {
        self.inner.released.lock().await.clone()
    }

    /// Give `scrollback(key)` a canned payload — the mock equivalent
    /// of tmux's retained pane history. The reported seq is the
    /// session's live high-water mark at call time, like the real
    /// backend.
    pub async fn set_deep_scrollback(&self, key: &str, bytes: impl AsRef<[u8]>) {
        self.inner
            .deep_scrollback
            .lock()
            .await
            .insert(key.into(), bytes.as_ref().to_vec());
    }
}

impl SessionBackend for MockBackend {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn spawn<'a>(
        &'a self,
        argv: &'a [String],
        cwd: Option<&'a Path>,
        env: &'a [(String, String)],
        hint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let delay = *self.inner.spawn_delay.lock().await;
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let n = self.inner.counter.fetch_add(1, Ordering::SeqCst);
            // Safe-ish hint substring — keeps test output readable.
            let safe_hint: String = hint
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            let key = format!("mock-{safe_hint}-{n}");
            let session = MockSession {
                argv: argv.to_vec(),
                env: env.to_vec(),
                cwd: cwd.map(|p| p.to_path_buf()),
                writes: Vec::new(),
                resizes: Vec::new(),
                replay: Vec::new(),
                last_seq: 0,
                subscribers: Vec::new(),
                exit_code: None,
                exit_waiters: Vec::new(),
            };
            self.inner
                .sessions
                .lock()
                .await
                .insert(key.clone(), session);
            Ok(key)
        })
    }

    fn write<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            self.inner.write_attempts.lock().await.push(key.into());
            let active = self.inner.active_writes.fetch_add(1, Ordering::SeqCst) + 1;
            self.inner
                .max_concurrent_writes
                .fetch_max(active, Ordering::SeqCst);
            let _active_call = ActiveWriteCall {
                inner: self.inner.clone(),
            };
            let wedged = self.inner.wedged_write_keys.lock().await.contains(key);
            if wedged {
                std::future::pending::<()>().await;
            }
            let delay = self.inner.write_delays.lock().await.get(key).copied();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let mut map = self.inner.sessions.lock().await;
            let session = map
                .get_mut(key)
                .ok_or_else(|| BackendError::NotFound(key.into()))?;
            session.writes.push(bytes.to_vec());
            Ok(())
        })
    }

    fn resize<'a>(
        &'a self,
        key: &'a str,
        cols: u16,
        rows: u16,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let wedged = self.inner.wedged_resize_keys.lock().await.contains(key);
            if wedged {
                std::future::pending::<()>().await;
            }
            let mut map = self.inner.sessions.lock().await;
            let session = map
                .get_mut(key)
                .ok_or_else(|| BackendError::NotFound(key.into()))?;
            session.resizes.push((cols, rows));
            Ok(())
        })
    }

    fn kill<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(message) = self.inner.kill_errors.lock().await.get(key).cloned() {
                return Err(BackendError::Other(message));
            }
            // Idempotent: missing key is fine.
            let mut map = self.inner.sessions.lock().await;
            if let Some(session) = map.get_mut(key)
                && session.exit_code.is_none()
            {
                session.exit_code = Some(-1);
                session.subscribers.clear();
                for waiter in std::mem::take(&mut session.exit_waiters) {
                    let _ = waiter.send(Some(-1));
                }
            }
            Ok(())
        })
    }

    fn release<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.inner.released.lock().await.push(key.to_string());
        })
    }

    fn list<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let map = self.inner.sessions.lock().await;
            Ok(map.keys().cloned().collect())
        })
    }

    fn subscribe<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Subscription, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let mut map = self.inner.sessions.lock().await;
            let session = map
                .get_mut(key)
                .ok_or_else(|| BackendError::NotFound(key.into()))?;
            let replay = session.replay.clone();
            let last_seq = session.last_seq;
            let (tx, rx) = mpsc::channel(crate::backend::SUBSCRIPTION_CHANNEL_CAPACITY);
            // If the session has already exited, close the channel
            // immediately so the subscriber's `recv()` returns None
            // straight away — matches the real backend's contract.
            if session.exit_code.is_none() {
                session.subscribers.push(tx);
            } else {
                drop(tx);
            }
            Ok(Subscription {
                replay,
                replay_complete: true,
                last_seq,
                live: rx,
            })
        })
    }

    fn snapshot<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<
        Box<dyn Future<Output = Result<crate::backend::ReplaySnapshot, BackendError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // `wedge_snapshot(key)` makes this future never resolve —
            // simulating a wedged tmux pump holding the ring mutex.
            if self.inner.wedged_snapshot_keys.lock().await.contains(key) {
                std::future::pending::<()>().await;
            }
            {
                let mut failures = self.inner.snapshot_failures.lock().await;
                if let Some(remaining) = failures.get_mut(key)
                    && *remaining > 0
                {
                    *remaining -= 1;
                    return Err(BackendError::Other("injected snapshot failure".into()));
                }
            }
            let map = self.inner.sessions.lock().await;
            let session = map
                .get(key)
                .ok_or_else(|| BackendError::NotFound(key.into()))?;
            Ok(crate::backend::ReplaySnapshot {
                replay: session.replay.clone(),
                last_seq: session.last_seq,
                complete: true,
            })
        })
    }

    fn scrollback<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(Vec<u8>, u64)>, BackendError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(bytes) = self.inner.deep_scrollback.lock().await.get(key).cloned() else {
                return Ok(None);
            };
            let map = self.inner.sessions.lock().await;
            let session = map
                .get(key)
                .ok_or_else(|| BackendError::NotFound(key.into()))?;
            Ok(Some((bytes, session.last_seq)))
        })
    }

    fn wait_exit<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<i32>> + Send + 'a>> {
        Box::pin(async move {
            let rx = {
                let mut map = self.inner.sessions.lock().await;
                let Some(session) = map.get_mut(key) else {
                    return None;
                };
                if let Some(code) = session.exit_code {
                    return Some(code);
                }
                let (tx, rx) = oneshot::channel();
                session.exit_waiters.push(tx);
                rx
            };
            // Mutex dropped — now we can await without blocking
            // emit/finish/kill on the same backend.
            rx.await.ok().flatten()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// Outer bound for every test in this module. Per the project
    /// rule: every async test wraps its body in `timeout(...)` so a
    /// deadlock surfaces as a failed test, not a hung suite.
    async fn run<F: std::future::Future<Output = ()>>(f: F) {
        timeout(Duration::from_secs(5), f)
            .await
            .expect("test deadline exceeded — likely a deadlock");
    }

    #[tokio::test]
    async fn spawn_returns_unique_keys() {
        run(async {
            let b = MockBackend::new();
            let a = b.spawn(&argv("echo a"), None, &[], "t").await.unwrap();
            let z = b.spawn(&argv("echo z"), None, &[], "t").await.unwrap();
            assert_ne!(a, z);
            assert!(a.contains("mock-"));
        })
        .await;
    }

    #[tokio::test]
    async fn list_reflects_live_sessions() {
        run(async {
            let b = MockBackend::new();
            let a = b.spawn(&argv("a"), None, &[], "alpha").await.unwrap();
            let z = b.spawn(&argv("z"), None, &[], "zulu").await.unwrap();
            let mut got = b.list().await.unwrap();
            got.sort();
            let mut want = vec![a, z];
            want.sort();
            assert_eq!(got, want);
        })
        .await;
    }

    #[tokio::test]
    async fn emit_then_subscribe_returns_replay_and_no_live_chunks() {
        run(async {
            let b = MockBackend::new();
            let k = b.spawn(&argv("x"), None, &[], "t").await.unwrap();
            b.emit(&k, b"hello").await;
            b.emit(&k, b"world").await;
            let mut sub = b.subscribe(&k).await.unwrap();
            assert_eq!(sub.replay, b"helloworld");
            assert_eq!(sub.last_seq, 2);
            // No subscribers were registered before the emits; live
            // channel is open but holds nothing.
            assert!(
                timeout(Duration::from_millis(20), sub.live.recv())
                    .await
                    .is_err()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn subscribe_then_emit_streams_live_chunks() {
        run(async {
            let b = MockBackend::new();
            let k = b.spawn(&argv("x"), None, &[], "t").await.unwrap();
            let mut sub = b.subscribe(&k).await.unwrap();
            b.emit(&k, b"hi").await;
            let chunk = sub.live.recv().await.expect("chunk");
            assert_eq!(chunk.bytes, b"hi");
            assert_eq!(chunk.seq, 1);
        })
        .await;
    }

    #[tokio::test]
    async fn finish_closes_subscribers_and_unblocks_wait_exit() {
        run(async {
            let b = MockBackend::new();
            let k = b.spawn(&argv("x"), None, &[], "t").await.unwrap();
            let mut sub = b.subscribe(&k).await.unwrap();
            // Park wait_exit in another task; finish should unblock it.
            let b2 = b.clone();
            let k2 = k.clone();
            let wait = tokio::spawn(async move { b2.wait_exit(&k2).await });
            b.finish(&k, 7).await;
            assert_eq!(wait.await.unwrap(), Some(7));
            // Subscriber sees channel close.
            assert!(sub.live.recv().await.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn write_and_resize_are_recorded() {
        run(async {
            let b = MockBackend::new();
            let k = b.spawn(&argv("x"), None, &[], "t").await.unwrap();
            b.write(&k, b"prompt\n").await.unwrap();
            b.resize(&k, 100, 30).await.unwrap();
            assert_eq!(b.writes_for(&k).await, vec![b"prompt\n".to_vec()]);
            assert_eq!(b.resizes_for(&k).await, vec![(100, 30)]);
        })
        .await;
    }

    #[tokio::test]
    async fn write_resize_on_missing_session_errors() {
        run(async {
            let b = MockBackend::new();
            let err = b.write("nope", b"x").await.expect_err("not found");
            assert!(matches!(err, BackendError::NotFound(_)));
        })
        .await;
    }

    #[tokio::test]
    async fn kill_is_idempotent() {
        run(async {
            let b = MockBackend::new();
            let k = b.spawn(&argv("x"), None, &[], "t").await.unwrap();
            b.kill(&k).await.unwrap();
            // Second kill on a closed session must not error.
            b.kill(&k).await.unwrap();
            // Missing key also fine.
            b.kill("nope").await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn injected_kill_failure_keeps_session_retryable() {
        run(async {
            let b = MockBackend::new();
            let k = b.spawn(&argv("x"), None, &[], "t").await.unwrap();
            b.fail_kill(&k, "transport timed out").await;

            assert!(b.kill(&k).await.is_err());
            assert!(
                b.list().await.unwrap().contains(&k),
                "a failed kill must preserve the backend slot for retry"
            );
        })
        .await;
    }
}
