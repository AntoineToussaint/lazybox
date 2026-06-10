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
use tokio::sync::Mutex;

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Internal map: backend session key → live PTY + cached exit code.
struct Slot {
    pty: Arc<DaemonPty>,
    /// Once the child exits, the pump task that drains its output
    /// stores the exit code here so subsequent `wait_exit` calls
    /// return immediately. `None` while the child is alive.
    exit: Arc<Mutex<Option<Option<i32>>>>,
}

pub struct RawPtyBackend {
    sessions: Mutex<HashMap<String, Slot>>,
    next_key: AtomicU64,
}

impl RawPtyBackend {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_key: AtomicU64::new(1),
        }
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
        Box::pin(async move {
            let size = PtySize {
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                pixel_width: 0,
                pixel_height: 0,
            };
            let pty = DaemonPty::spawn(
                argv,
                size,
                cwd.map(|p| p.to_path_buf()).as_ref(),
                env.to_vec(),
            )
            .map_err(|e| BackendError::Spawn(e.to_string()))?;
            let pty = Arc::new(pty);
            let key = self.alloc_key(hint);
            let slot = Slot {
                pty: pty.clone(),
                exit: Arc::new(Mutex::new(None)),
            };
            // Background task that watches for exit and caches the
            // code. Without this, only ONE call to wait_exit ever
            // succeeds (DaemonPty's oneshot is consumed). The trait
            // contract is "call repeatedly, get the cached code".
            let exit_slot = slot.exit.clone();
            let pty_for_exit = pty.clone();
            tokio::spawn(async move {
                let code = pty_for_exit.wait_exit().await;
                *exit_slot.lock().await = Some(code);
            });
            self.sessions.lock().await.insert(key.clone(), slot);
            Ok(key)
        })
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
            // Remove the slot so subsequent calls are no-ops. SIGTERM
            // the child via DaemonPty::kill — the output pump task
            // observes the broadcast close and emits TerminalExited
            // upstream.
            if let Some(slot) = self.sessions.lock().await.remove(key) {
                slot.pty.kill();
            }
            Ok(())
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

    fn snapshot<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(Vec<u8>, u64), BackendError>> + Send + 'a>> {
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
            // instead of buffering without limit — the consumer's
            // seq-gap detection resyncs from the replay ring.
            let mut sub = pty.subscribe().await;
            let replay = std::mem::take(&mut sub.replay);
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
            let exit = {
                let map = self.sessions.lock().await;
                map.get(key).map(|s| s.exit.clone())?
            };
            // Poll the cached slot. The spawn task fills it on exit;
            // until then we yield and re-check. Cheap because the
            // slot is a small mutex and exits are rare.
            loop {
                if let Some(code) = *exit.lock().await {
                    return code;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    }
}
