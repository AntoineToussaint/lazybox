# lazybox architecture — robustness and performance

This document details the architectural improvements and design patterns that enable lazybox to stay responsive and reliable under load. For high-level architecture, see [`DESIGN.md`](../DESIGN.md). For day-to-day conventions, see [`CLAUDE.md`](../CLAUDE.md).

## Terminal robustness

### Ring buffer integrity

The embedded terminal (each PTY session) holds output in a ring buffer to enable replay when clients reconnect or resync. The improvements in this area prevent silent data loss and exhaustion.

**Size validation at initialization:**

The ring buffer capacity is now validated at creation time with hard limits:

```rust
const MAX_RING_SIZE: usize = 100 * 1024 * 1024;  // 100 MiB per terminal

// Panics if:
// - capacity == 0 (would silently drop all output)
// - capacity > MAX_RING_SIZE (would exhaust system memory)
```

This prevents misconfiguration from silently losing output. A zero-size ring would accept writes but never retain them — tests now ensure this is caught immediately.

**Per-terminal byte ceiling:**

Terminal VT buffers are capped at 64 MiB per slot (down from 195 MiB). If an agent crashes and continues writing indefinitely, or output becomes pathological, the ceiling prevents runaway memory. Exceeding the ceiling causes the terminal to drop its VT parser and render a freeze-frame, preserving the last-known content while freeing memory.

### Resync and DEC-mode synchronization

When a TUI client reconnects or requests a terminal state snapshot, the server must send a replay of recent terminal output. Two issues were fixed:

**Per-terminal resync gating:**

Previously, if one terminal's ringbuffer fell behind (e.g., a slow client or congested connection), the resync request would create debt that fed back into the event loop, triggering cascading resync requests across *all* terminals. This created a resync storm.

Now, resync debt is tracked **per-terminal**. One congested terminal's backpressure does not convert other terminals' output into resync requests. Each terminal independently manages its own replay debt:

- **Pace-based delivery:** Resync replays are paced (debounced at 1s minimum) rather than sent on every request. If multiple resync requests arrive in quick succession, they're coalesced into one outgoing replay.
- **Budget enforcement:** Terminal replays are capped by the same budget that bounds Subscribe/bus-lag snapshots (`SNAPSHOT_REPLAY_BUDGET`). An over-budget replay is announced unavailable in full (never torn), so the client knows to re-request rather than apply a corrupt prefix.

**DEC-mode tracking:**

The VT parser tracks terminal modes (bold, underline, color, charset, etc.) that change as sequences are applied. The server and client must stay in sync. Drift was caused by:

1. **Missing EOF trim:** Sequences were sometimes left incomplete in the ring buffer, leaving the client's parser in the wrong mode for subsequent output.
2. **Partial replays:** When a replay was split (due to size limits), the client VT ended up in the middle of a sequence.

Now:
- Terminal output is always properly terminated (EOF trim removes incomplete ANSI sequences).
- Replays that exceed budget are omitted **whole** and announced unavailable, never sliced.
- **Resize fencing:** When the terminal resizes, the output sequence is synchronously fenced with a `\e[2J` (clear screen) to prevent mid-redraw corruption. The client applies the resize, then re-requests the full replay.

---

## Event bus and backpressure

### Bus-lag recovery asynchrony

When a client lags behind the server's event bus (e.g., a slow SSH connection), the server snapshots the current state and sends it to the client to resynchronize. This snapshot building — especially `snapshot_terminals()` which collects all ringbuffers — was synchronous and ran on the serve loop, causing **up to 4-second freezes**.

**Moving recovery off the serve loop:**

Snapshot building is now spawned as a background task in the mutations `JoinSet`:

```rust
// Before: synchronous, blocks the serve loop for ~4s
let snapshot = snapshot_terminals();
send_snapshot_event(snapshot);

// After: async, returns immediately
spawn_snapshot_task_async(Arc::clone(&self.generation), snapshot_rx);
```

A **generation counter** (`Arc<AtomicU64>`) sequences the tasks:

1. Each background snapshot task increments its own "I am generation N" counter.
2. If a newer recovery is spawned while an older one is still building, the older task detects this and discards its snapshot (no send).
3. Only the latest snapshot is ever sent, preventing stale state from overwriting newer data.
4. Debouncing ensures at most 2 seconds between recovery attempts (`LAG_RECOVERY_DEBOUNCE`).

This keeps the serve loop responsive while recovery happens in parallel.

### Polling backoff

The polling scheduler wakes at fixed 5-second intervals to check all providers (GitHub, Linear, Slack). On an idle inbox with many repos, this creates constant CPU wakeups even when there's no data to fetch.

**Exponential backoff on empty polls:**

When consecutive polls return no data:

1. **Double the interval** on each empty poll (5s → 10s → 20s → … → 150s max).
2. **Reset to base** (5s) as soon as data arrives or the user triggers a refresh.

The backoff multiplier is tracked in `TickState` and persists across poll cycles:

```rust
struct TickState {
    backoff_multiplier: u32,  // 1..=30
}

// On empty poll:
state.update_backoff(PolledItems::empty());  // multiplier *= 2, capped at 30

// On data or refresh:
state.update_backoff(PolledItems::some());   // multiplier = 1
```

This dramatically reduces idle CPU (from constant wakeups to nearly dormant) while remaining instant-responsive to activity or user action.

---

## Lock optimization and contention

### Registry lock scope reduction

The provider registry (which tracks active GitHub/Linear/Slack clients and their state) was being held across slow operations like file I/O (persisting workspace metadata). This serialized unrelated operations.

**One acquisition per chunk:**

Now the lock is acquired, the required data is copied, and the lock is immediately released before any I/O:

```rust
// Before: lock held across persist
let lock = registry.write();
let data = lock.fetch_data();
fs::write(data)?;  // while lock is held!

// After: acquire, copy, release
let data = {
    let lock = registry.read();
    lock.fetch_data()  // copy, then lock drops
};
fs::write(data)?;  // no lock held
```

This pattern is applied per chunk so that slow I/O (especially on network filesystems) doesn't block all other provider operations.

### Keystroke path off the global queue

Keystroke handling was queued behind the same global lock as polling, snapshotting, and workspace mutations. A heavy background task would stall the keystroke path.

**Separate paths:**

Keystroke input (and associated config writes) now bypasses the global-lock queue. This ensures the UI remains responsive even during heavy polling or snapshot operations.

---

## Liveness and resource governance

### Force-quit safeguard

If the UI thread's run-loop heartbeat becomes stale (>2s without a tick), pressing **Ctrl-C three times rapidly** will:

1. Restore terminal mode (exit raw TTY mode).
2. Terminate from the input reader thread directly.
3. Avoid waiting for the hung run loop.

The safeguard is inactive while the loop is healthy (normal Ctrl-C behavior), so agent interrupts still forward correctly.

### Resource limits

**Agent spawn cap:** `agent.max_live_agents` (default: 32) prevents spawning beyond a configured limit. Recovery re-attaches existing agents but warns loudly if over cap.

**Terminal memory:** VT buffers are capped at 64 MiB. Crashed agents drop their parser and render a freeze-frame.

**Hook connection slots:** Hook spawns use a pooled connection-slot model (default: 4 concurrent) to prevent file-descriptor exhaustion.

---

## Resilience patterns

### Credential caching and backoff

GitHub API credentials are cached (5-minute TTL) with exponential failure backoff. If credential resolution fails:

1. Fall back to the cached credential from the last successful fetch.
2. Exponentially back off on repeated failures (5s → 10s → 30s…).
3. Surface failures on the event bus instead of silent `info!()` logging.
4. Explicit `Shift-R` refresh clears the cache and forces a retry.

Linear polls re-arm their cadence on failure (rather than getting stuck in retry loop).

### Config caching

`Config::load()` is stamp-cached — the parsed config is cached if the file mtime hasn't changed. This reduces lock contention on the ~30 hot call sites that check config during keystroke dispatch.

---

## Architectural invariants

### Structural typing via traits

- **`TaskProvider`:** GitHub, Linear, Slack all implement one interface.
- **`CredentialProvider`:** Env, Command, Static, and custom providers all plug into one chain.
- **`Agent`:** Claude, Codex, Cursor, GenericCli all implement one spawn/resume contract.
- **`Store`:** SQLite is the default; Postgres or in-memory backends can be swapped.

This enables graceful degradation (a provider fails, others continue) and testing (mock implementations).

### Client/daemon abstraction

The client and daemon communicate via the `Client` trait:

```rust
pub trait Client {
    async fn send_command(&self, cmd: Command) -> Result<()>;
    fn subscribe(&self) -> Receiver<Event>;
}
```

Local clients use an `mpsc::channel` pair (zero-copy). Remote clients serialize over a Unix socket. The TUI code doesn't branch on locality.

---

## Testing and validation

Every fix carries regression tests:

- **Ring buffer:** Tests for zero-size, over-max, and max-size rings.
- **Polling backoff:** Tests for empty/data transitions and multiplier bounds.
- **Resync gates:** Tests that congestion on one terminal doesn't cascade to others.
- **Archive reconciliation:** Tests that taskless workspaces are only reaped if truly empty.
- **Liveness:** Tests for the Ctrl-C timeout and heartbeat staleness detection.

The full workspace test suite runs 5,473 tests and is green on every build.

---

## See also

- [`TROUBLESHOOTING.md`](TROUBLESHOOTING.md) — user-facing guidance for common issues.
- [`DESIGN.md`](../DESIGN.md) — high-level design goals and the client/daemon split.
- [`CLAUDE.md`](../CLAUDE.md) — build, run, and contributor conventions.
