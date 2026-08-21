# Troubleshooting lazybox

This guide covers common issues and their solutions. For architectural details on the fixes described here, see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## High CPU usage on idle

**Symptoms:** The lazybox process shows elevated CPU even when the inbox is idle and no agents are running.

**Root cause:** The polling loop was waking up at fixed intervals (5s) and performing full scans of all providers (GitHub, Linear, etc.) regardless of whether there was any data to fetch. Over time with many repos this becomes expensive.

**Solution:** Lazybox now implements **exponential backoff** on the polling scheduler. When consecutive polls return no data, the tick interval doubles (up to a 30x maximum) to reduce CPU usage during idle periods. The interval resets to the base 5s tick as soon as new data arrives or the user navigates to trigger a refresh.

**What to do:**
- No action required — this is automatic. During idle periods, polling gradually backs off.
- If you notice activity is slow to appear, press `Shift-R` to force an immediate refresh (this resets the backoff and triggers a full poll).
- The sync-status screen (`Shift-D`) shows the current polling state and can help diagnose if polls are being throttled.

---

## Reconnect freeze or slow recovery after disconnect

**Symptoms:** After reconnecting a remote TUI client (e.g., SSH tunnel) or losing network briefly, the UI hangs for seconds. Keystrokes are unresponsive until recovery completes.

**Root cause:** The server was building large recovery snapshots (especially of terminal ringbuffers, which can be ~4s of data per terminal) synchronously on the serve loop. This blocked all command dispatch, keystroke processing, and event delivery until the snapshot was ready.

**Solution:** Recovery snapshots are now built asynchronously in a background task, off the serve loop. The serve loop remains responsive to keystrokes and commands while recovery happens in parallel. Multiple recovery requests in quick succession are coalesced (only the latest snapshot is sent; older ones are discarded).

**What to do:**
- No action required — keystrokes and commands will flow during recovery.
- A notice in the footer indicates when lag recovery is in progress.
- If recovery is needed repeatedly (e.g., unstable network), the most recent snapshot will be delivered once ready.

---

## Terminal output appears corrupt or incomplete

**Symptoms:** The agent terminal shows garbled text, missing output, or rendering artifacts (e.g., boxes not drawn correctly, text overlapping).

**Root cause:** Multiple issues were fixed in the terminal output pipeline:
1. **Resync storm:** When one terminal's buffer fell behind, the system would repeatedly ask for resync, creating a cascade that stalled other terminals.
2. **Missing EOF trim:** Terminal output wasn't being properly terminated, leaving partial ANSI sequences in the buffer.
3. **DEC-mode desynchronization:** The VT parser and the server's understanding of terminal modes could drift, causing subsequent output to be misinterpreted.

**Solution:**
- **Per-terminal resync gates:** Each terminal now manages its own resync debt independently. One congested terminal no longer triggers resync cascades across all other terminals.
- **EOF trim and DEC-mode sync:** The terminal output pipeline now properly terminates sequences and keeps the VT parser in sync with the server's mode state.
- **Resize fencing:** Terminal resize events are properly sequenced with output to prevent mid-redraw corruption.

**What to do:**
- Press `Ctrl-L` (or `]]` then `f` to toggle focus mode) to trigger an explicit full redraw. If the issue persists, it indicates a deeper problem.
- Check the logs at `/tmp/lazybox.log` for errors related to terminal resync or VT parsing.
- If output corruption is reproducible, report it with the agent's stdout and the log snippet.

---

## Terminal is slow or unresponsive with many lifecycle hooks

**Symptoms:** When you have many hooks configured (e.g., pre-spawn, post-spawn, on-activity, etc.), the terminal becomes sluggish or commands hang.

**Root cause:** Each hook spawn was creating a new connection and competing with the main application for file descriptors. Under load, the connection queue could exhaust available slots, causing hook operations to block or fail.

**Solution:** Hook spawns now use a fixed connection-slot pool (default: 4 concurrent hook connections). Once slots are exhausted, new hook invocations queue and execute when a slot becomes available. This prevents file-descriptor exhaustion and keeps the UI responsive.

**What to do:**
- If you have many hooks, you can tune the pool size in your config:
  ```yaml
  server:
    hook_connection_slots: 8  # increase if hooks queue too often
  ```
- Monitor the sync-status screen (`Shift-D`) to see if hooks are frequently queued.
- If hooks are frequently blocked, increase the pool size (at the cost of more concurrent spawns).

---

## Activity pane shows missing or stale activity

**Symptoms:** Comments, CI results, or review requests appear in the web interface but don't show in lazybox's activity pane. Refreshing (`Shift-R`) sometimes fixes it, sometimes doesn't.

**Root cause:** The polling system was caching the archived status of tasks at poll time. If a PR was reopened or unarchived after being fetched, the cache wouldn't update, and subsequent polls would skip it entirely.

**Solution:** Archive status is now checked **per-item** during each poll, not snapshotted once. Additionally, session reaping (cleanup of old workspaces) now respects the current state of tasks to avoid deleting active workspaces whose PR/issue was reopened.

**What to do:**
- Press `Shift-R` to force a full refresh if activity is missing. This clears caches and re-polls all sources.
- If a specific PR/issue remains missing after refresh, check that:
  - It's not archived or deleted in the source system (GitHub, Linear, etc.)
  - You have permission to view it
  - The source is enabled in your config (`~/.lazybox/config.yaml`)
- If you frequently see stale activity, the source may be flaky — check its status or increase the refresh interval in your config.

---

## Other common issues

### Worktre creation fails with "too many open files"

This can happen if many agents are running concurrently. Each worktree and PTY requires file descriptors.

**Solution:**
- Increase your system's file descriptor limit: `ulimit -n 8192` (or higher).
- Reduce the number of concurrent agents using the `agent.max_live_agents` config (default: 32).

### Configuration file not reloaded

If you edit `~/.lazybox/config.yaml` and the changes don't appear, they're being loaded but not reflected in a running session.

**Solution:**
- Restart lazybox: `q q` to quit, then `lazybox` to run again. Configuration is loaded on startup.
- Config changes made in the settings menu (`,` key) are saved automatically.

### State database corruption

You see errors about the SQLite database or loss of read/unread state.

**Solution:**
- The state database lives at `~/.lazybox/v2/state.db`. Try backing it up and deleting it:
  ```bash
  mv ~/.lazybox/v2/state.db ~/.lazybox/v2/state.db.bak
  lazybox  # will create a fresh state.db
  ```
- If the issue persists, report it with the `.bak` file for analysis.

---

## Getting help

If you encounter an issue not covered here:

1. Check the logs: `tail -f /tmp/lazybox.log`
2. Run the test suite: `cargo test --workspace` to check for local issues.
3. Open an issue on [GitHub](https://github.com/AntoineToussaint/lazybox/issues) with:
   - The output from `/tmp/lazybox.log` (the last 50 lines at the time of the issue)
   - Your `~/.lazybox/config.yaml` (with secrets redacted)
   - Steps to reproduce
4. Or reach out on [GitHub Discussions](https://github.com/AntoineToussaint/lazybox/discussions) for open-ended questions.
