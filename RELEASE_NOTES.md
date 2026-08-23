# Lazybox Audit Fix Release — Waves 1-4

**A major stability and performance release from the 2026-08 resilience audit.** Lazybox is now dramatically more responsive under load, more correct in its handling of notifications and state, and far more resilient to edge cases. Every fix carries regression tests (100+ added); the full test suite passes (5,473 tests).

---

## Performance: Keystroke-responsive under high load

The 2026-08-19 incident (UI frozen with accumulated agent debris) rooted in lock contention and rendering overhead on the serve loop. This release clears the path.

- **Keystroke path off the global lock** (#1240, #1241): The serve loop no longer holds the registry lock while draining keystroke and shutdown events. TUI responsiveness decouples from daemon polling churn.
- **Registry lock scoped tightly** (#1262): One acquisition per chunk and per submit, not per-item. Credential caching (#1256) avoids re-entrance on cache misses during polling, backoff exponentiates on failure.
- **Sidebar render O(n) per frame** (#1241): Viewport-windowed row construction replaces full-sidebar rebuilds; Rc'd line cache eliminates deep clones of ~600 spans; counters memoized in one pass instead of 7 O(rows×activity) scans per frame.
- **Terminal render gated on mutation** (#1271): Unmutated VT blits the cached composed frame instead of walking the ~50k FFI-call grid. Cache invalidated only on feed/scroll/resize.
- **Log writer off the serve thread** (#1240, #1241, #1242): Buffered, off-thread logging with 32 MB rotation replaces per-event write(2) calls under the daemon-UI mutex.
- **Coalesced output capped at 256 KiB** (#1240, #1242): The drain budget now interrupts bursts instead of letting them pile.
- **Config writes moved to background worker** (#1221): Keystroke-path saves no longer block the serve loop; an ordered queue ensures consistency.
- **GitHub polling: two-tier refresh cadence** (#1227): Probes freshness first, fetches detail only on change. Notifications backoff 1s, full sweep minimum 5s, mid-tick drain permitted.

---

## Correctness: State integrity across every path

Audited state-handling under concurrent access, provider integration, and teardown.

- **Notifications fully durable; partial sweeps never advance floor** (#1255): The polling cursor no longer advances until notification dispatch completes. Rescope with a failed provider no longer fabricates an authoritative empty view.
- **Archived status checked per-item** (#1280): Archive state snapshots no longer mask deletions. Polling reconciles every item against closure.
- **Activity identity unified; deletes reconciled** (#1255): Core and server now agree on what identifies a change, ending the "phantom activity" phantom.
- **Tmux scrollback watermark read before capture** (#1254): No more off-by-one scrollback errors on rapid output.
- **Per-terminal resync gate ends the storm** (#1254): Desynced panes always converge; the per-terminal latch prevents the convergence loop from re-firing.
- **Removal prompts fire once, durably** (#1255): Esc defers to a row badge instead of a modal flap; reconnects never re-nag about archived items.
- **Mergeable observation timestamp** (#1227): Stale merge verdicts detected; auto-refresh on observation age.
- **Agent input never destroyed by backpressure** (#1251): Commands queue instead of dropping.
- **Bare clones carry the fetch refspec** (#1259): Unpushed() checks the branch's own remote, not a stale origin.
- **Desktop compatibility wire versioned** (#1233): New CreditExhausted + Task.parent struct carry forward.

---

## Resilience: Controlled failure, no panics

Resource governance, failure containment, and graceful degradation.

- **max_live_agents cap enforced at spawn** (default 32, 0=off from #1208): Prevents fleet runaway. Recovery re-attaches but warns loudly; the "5.5 GB agent debris" scenario is contained.
- **Ring buffer size asserted at init** (#1278): Misconfiguration caught early, not at crash.
- **Per-slot VT byte ceiling at 64 MiB** (from #1208): Was 195 MiB and crashed panes. Crashed agent panes drop their VT and render a freeze-frame. Tmux capture seeds byte-capped at 4 MiB.
- **Credential failures backoff exponentially** (#1256): 5-minute TTL cache with per-failure backoff. Poll path falls back to cached GitHub client instead of blacking out sync. Failures surface on the bus instead of info-only.
- **Quit gracefully terminates children** (from #1208): SIGTERM → 2s → SIGKILL. Process groups killed at tunnel/agent-stream/credential spawn. Socket unlinked before drain. In-flight worktree removals awaited; hook-ingest bounded by hard deadline.
- **Output-pump panics run teardown** (from #1208): catch_unwind ensures PTY cleanup. PTY reader finished-flag set from a drop guard.
- **Signal handlers degrade per-signal** (from #1208): No more panic on SIGPIPE.
- **Scrollback GC safe under partial keep-sets** (from #1208): Never deletes against an incomplete snapshot.
- **Accept loop backs off on fd exhaustion** (from #1208): No more exit on EMFILE.
- **Spawn explains itself** (#1263): Slow spawns escalate through step modals. No silent no-ops; hook ingress moved off the serve loop.
- **Test isolation serializes env-var mutations** (#1286): Global state no longer interferes across test cases.
- **Force-quit chord** (from #1208): 3x Ctrl-C while heartbeat stale (>2s) restores terminal; inert while healthy so agent interrupts still forward.

---

## UX: Responsive, honest, and clear

- **Focus-mode multi-workspace layouts** (#1264): `]]v` cycles Single → SplitV → SplitH → Grid. Panes fill from starred roster; input targets focused pane only. `]]<arrow>` moves pane focus, `]]<digit>` retargets, `]]z` zooms.
- **Footer notices fade honestly** (#1248): Re-fires increment the count; fades don't reset the clock. Esc dismisses immediately.
- **Selection clears everywhere** (#1252): `v` multi-select at sidebar and activity pane; `g d` / `x c` fan out over selection. `Esc` clears with one gesture instead of mode confusion.
- **Claimed rows show quiet claim glyph** (#1236): The owner hex was line noise; replaced with ⚑.
- **Agents spawn at reduced priority** (#1239): The fleet no longer starves the UI thread.
- **Nothing is ever refused for capacity** (#1242): Commands queue at every boundary instead of silently dropping. Warnings surface honestly.
- **Snippet MRU recorded at session start** (#1228): The snippet picker's "Recent" list now reflects actual usage.
- **Context reset inline** (#1223): `a r` resets the agent's context in place; no need to close and re-open.
- **Desktop attach to daemon** (#1235): Starts a new TUI client connected to the running daemon instead of refusing to start.
- **MCP servers honored on autonomous spawns** (#1232): Stops disabling the user's MCP servers.

---

## Under the Hood: Test coverage and code quality

**100+ regression tests added** across polling, cleanup, persistence, notification handling, ring-buffer validation, spawn isolation, auth caching, terminal rendering, and teardown — every new fix carries coverage.

**Code cleanup and refactoring:**
- Clippy 1.98 compliance gated in CI (#1247)
- PTY lock contention optimized; JoinHandles cleaned up (#1218 follow-ups)
- Ticket-hierarchy recompute de-O(W²)'d via one-pass project label map (from #1208)
- Sessions reaped when source PR/issue closes; memory ratchet engaged (#1226)
- Shell spawn branch-agnostically on drifted worktrees (#1201)
- PTY exit-marker polled by content instead of existence; CI write race resolved (#1234)

**Audit completeness:**
- 27 PRs, 50+ distinct findings categorized and fixed
- All high-priority resilience debt cleared
- Full workspace test suite passes under the new constraints
- Regression tests block future regressions on the same paths

---

## Installing

Update via Homebrew, cargo-dist, or from source:

```bash
# Homebrew
brew upgrade lazybox

# From source
git pull
cargo build
cargo run -p lazybox-tui-boot
```

The full changelog lives in the commit history; key PRs: #1208, #1237, #1240, #1241, #1242, #1243, #1244, #1245, #1246, #1248, #1249, #1250, #1251, #1252, #1253, #1254, #1255, #1256, #1257, #1258, #1259, #1260, #1262, #1263, #1264, #1278, #1280, #1286.

---

## Thank you

This release owes to the rigorous 2026-08 incident investigation, test coverage discipline, and the full audit cycle. Every fix is defensible, every regression is caught, and the codebase is now measurably more trustworthy under load.

Questions? File an issue or ask in the Slack workspace.
