# Deep review: UI stalls & run-loop frame-budget overruns

Issue: #1031. Related: #1030 (sync-driven freeze — one confirmed cause),
#585/#700 (poll load).

This is an audit, not a fix. It turns the run-loop watchdog's existing
warnings into a ranked, evidenced distribution of stall causes and proposes
one fix per cause. Each proposed fix is a candidate follow-up issue; none are
implemented here.

## Method

The run loop is already instrumented. Each iteration's work phase is timed and
broken into five segments — `dispatch / drain / ticks / messages / render` —
and `LoopWatchdog::observe` logs a rate-limited `warn` line whenever the phase
exceeds `FRAME_BUDGET` (50 ms), naming the `worst_phase` and every segment
(`crates/tui/src/realm/model/helpers.rs:871-909`). Those lines land in
`/tmp/lazybox.log`.

The evidence below comes from two sources:

1. **Telemetry** — aggregated from a real 61 MB `/tmp/lazybox.log` covering
   several days of sessions (4,105 over-budget iterations). This is the actual
   watchdog output, not a synthetic benchmark.
2. **Code paths** — the per-frame / per-event work each phase does, read
   directly, with `file:line` evidence.

## 1. Watchdog telemetry — ranked cause distribution

`worst_phase` across all 4,105 over-budget iterations:

| worst_phase | count | share |
|-------------|-------|-------|
| **render**   | 3,374 | **82.2 %** |
| ticks       |   366 |  8.9 % |
| drain       |   308 |  7.5 % |
| dispatch    |    41 |  1.0 % |
| messages    |    16 |  0.4 % |

**Render dominates by a wide margin.** The other phases are a long tail.

Over-budget iteration duration (all phases):

| iteration_ms | count |
|--------------|-------|
| 50–100   | 2,429 |
| 100–200  |   753 |
| 200–400  |   314 |
| 400–800  |   186 |
| 800 +    |   423 |

Render-phase stalls, split into real CPU cost vs. clock artifacts:

- **Real render stalls (`render_ms` < 5 s), n = 3,273:** p50 **81 ms**, p90
  **676 ms**, p99 **3,944 ms**, max 4,912 ms. Even the *median* over-budget
  render is 1.6× the frame budget; p90 is a clearly visible freeze.
- **Cumulative frozen wall-time from sub-5 s render stalls alone: ~1,005 s
  (~17 minutes)** across the log. This is the headline number — render is not
  just the most *frequent* cause, it is by far the largest *time sink*.
- **Suspend/blocked-write artifacts (`render_ms` ≥ 5 s), n = 104, totalling
  ~2,060 s.** These are not CPU: e.g. one `iteration_ms=195727` (195 s) line
  has `suppressed=2` — a single 195 s blocking call (or a suspend) produces one
  over-budget observation, not the thousands a genuine CPU spin would suppress.
  They cluster tightly in wall-clock (10 of the 18 stalls >30 s fall in one
  ~40-minute 14:0x–14:4x window), the signature
  of laptop **suspend/resume** spanning the monotonic clock, or a **blocked
  stdout write** to a stalled terminal/SSH pipe inside `terminal.draw`. They
  are excluded from the render-cost analysis but are a real *reporting*
  problem — see finding R-0.

**Storm signal.** `suppressed` (over-budget iterations swallowed by the 1 s
warn rate-limit) is mostly 0–4 but reaches 7, i.e. episodes of ~8 consecutive
over-budget frames. At p90 (~676 ms each) that is multiple seconds of
continuous freeze — long enough to age buffered input past the 500 ms stale
bound and drop it (confirmed in §6).

**Drain overflow.** 178 `re-synced the terminal grid` warnings: the daemon's
bounded event channel overflowed, dropped `TerminalOutput`, and forced a
grid rebuild. Zero `backlog growing` warnings — the bounded channel holds, but
a chatty agent does overrun it, and each resync forces an expensive full-grid
re-render (ties drain pressure back to the render cost).

## 2. Render phase — the dominant offender

`Model::view()` (`crates/tui/src/realm/model/mod.rs:4750`) rebuilds the **entire
widget tree every paint** — sidebar, activity pane, terminal stack, footer.
There is no dirty-region or per-pane change gate; ratatui diffs the finished
buffer against the previous one to minimise *terminal writes*, but the
*widget-build* cost (projection, markdown, grid walk) is paid in full on every
frame. Background frames are coalesced to ~60 fps by `RenderThrottle`
(`helpers.rs:1036-1060`), but that caps frame *rate*, not the cost of a single
frame — one expensive `view()` still blocks the thread for its whole duration.

### R-0. Multi-second "render" stalls are mis-attributed (reporting bug)

- **Evidence:** 104 stalls ≥ 5 s (up to 195 s) attributed to `worst_phase="render"`
  with tiny `suppressed` counts and tight wall-clock clustering (§1).
- **Cause:** the render bracket (`helpers.rs:1296-1312`) wraps `model.view()`,
  which includes the synchronous `terminal.draw` stdout flush. A process
  suspend, or a blocked write to a stalled tty/SSH pipe, lands inside that
  bracket and is charged to "render", polluting the render distribution and
  masking whether a real render or an external stall occurred.
- **Fix:** distinguish wall-clock from CPU (e.g. sample thread CPU time around
  the phase, or detect a monotonic-vs-wall gap) and tag suspend/blocked-write
  iterations as a separate category so they don't inflate render.
- **Tag:** render (reporting). **Priority: low** (accuracy, not UX).

### R-1. PR/issue body markdown re-rendered **twice per frame, uncached**

- **Evidence:** with the description expanded to `Preview`, `RightPane::render`
  calls `comment_render::render_body` on the **raw body string twice every
  frame** — once for sizing (`mod.rs:2373`, via `task_body_content_rows`) and
  once for drawing (`mod.rs:2432-2436`, via `render_task_body`).
  `render_body` (`components/comment_render.rs:135`) allocates two whole-body
  `String` copies (`:141`, HTML-comment + tag stripping), a `Vec<char>` per
  line (`:309`), and per-word `Span`/`String` in `wrap_one` (`:480-588`) with
  grapheme/visual-width measurement per word. Nothing memoises it.
- **Scaling:** O(body bytes), paid ×2 per frame. A large PR body with the
  description open is exactly the "300–450 ms render" the issue reports.
- **Fix:** memoise the rendered body keyed by `(body_rev, width)` — the exact
  pattern already used one file over for the activity feed
  (`activity_buffer`, `mod.rs:1783-1808`). Render once, size from the cache.
- **Tag:** render. **Priority: high** (biggest uncached per-frame scaler).

### R-2. Full-body scan `body_wants_rich_modal` every frame

- **Evidence:** `render_task_body` calls `wants_full_modal()` →
  `body_wants_rich_modal()` (`mod.rs:2553-2581`) every frame, scanning the
  whole body: `contains("![")`, a `body.lines()` fence-marker loop, and a
  second `body.lines().any(...)` table-delimiter scan doing `trim_start_matches`
  + `chars().all(...)` per line. O(body size), 2+ passes, uncached.
- **Fix:** compute once when the body changes (fold into the R-1 memo).
- **Tag:** render. **Priority: medium.**

### R-3. Sidebar render redoes ~6–7 full O(workspaces) scans per frame

The sidebar's expensive projection (sort / filter / group / **search & fuzzy
scoring**) is correctly cached in `self.visible` / `self.repo_summaries` and
only rebuilt on data change via `recompute_visible_inner`
(`components/sidebar/mod.rs:2386-2406`) — **not** at paint time. Good. But
`Sidebar::render` (`components/sidebar/render.rs:32`) still redoes, every frame:

- **R-3a — per-row badge scans, O(rows × terminals):** each visible row calls
  `runner_badges(key)` + `agent_models(key)` (`render.rs:1117,1119` →
  `mod.rs:1934-1978`), and **each rescans the entire `running_terminals` map**
  and allocates a `HashMap` (plus a sort). This is the worst scaling term:
  N rows × T terminals per frame. **Fix:** precompute a `SessionKey → badges`
  map once per frame (or on terminal change). **Priority: high.**
- **R-3b — three attention-counter passes:** `input_pending_count` /
  `ci_failing_count` / `review_pending_count` (`render.rs:50-52` →
  `mod.rs:1870-1910`) are three independent linear scans, each allocating a
  fresh `Vec<AttentionSignal>` per workspace
  (`crates/tui-core/src/inbox/attention.rs:136-171`) just to produce three
  integers. Plus `total_unread_count` (`render.rs:49`) is a fourth scan.
  **Fix:** one pass, or cache in `recompute_visible`. **Priority: medium.**
- **R-3c — full per-row widget rebuild:** `prebuild_workspace_lines`
  (`render.rs:540`) rebuilds a 13-cell `Row` per visible workspace every frame
  with many `format!`/`to_string`/pill allocations
  (`components/sidebar/workspace_row.rs:303-320` and pills), then a table
  width pre-pass (`table.rs:462-575`). None memoised. **Fix:** cache built
  `Line`s per row, invalidate on data/cursor/spinner/theme change.
  **Priority: medium.**
- **R-3d — misc per-frame allocations:** `agent_workspace_keys()`
  (`render.rs:950` → clones keys + scans sessions),
  `limit_reached_workspace_count` (`render.rs:135` → HashSet/frame),
  `visible_broadcast_selected_count` / `workspace_count`. Individually cheap,
  they stack. **Priority: low.**

### R-4. Reader modal clones the rendered doc every frame

- **Evidence:** the full-body markdown reader parses via pulldown-cmark only
  when width changes (`realm/components/markdown_modal.rs:140-143`) — good, the
  parse is cached — but every frame does `Paragraph::new(self.rendered.lines
  .clone())` (`markdown_modal.rs:151`), an O(doc size) allocation per frame
  while open. **Fix:** render the cached lines by reference / windowed to the
  viewport. **Tag:** render. **Priority: low.**

### R-5. Terminal grid walk — full viewport, ~5 FFI calls per cell, no dirty gate

- **Evidence:** `Model::view()` → `terminals.view_in` (`mod.rs:4987`) →
  `TerminalStack::render` → `render_one_terminal` (`components/terminal_stack.rs:4257`)
  → `GhosttyTerminal` widget (`crates/tui-term/src/ghostty_widget.rs:88`,
  loop `:169-297`). The widget walks **every** viewport row × column each
  frame, and per cell makes **separate FFI round-trips**: `graphemes_len`
  (`:198`), `graphemes_buf` (`:204`), `fg_color` (`:211`), `bg_color` (`:212`),
  `style` (`:219`). At the default 120×32 grid that is ~3,840 cells × ~5 =
  **~19k FFI calls per frame per visible tile**.
- **No dirty gate — deliberately:** `ghostty_widget.rs:174-176` states
  `row.dirty()` is intentionally not consulted; libghostty's per-row dirty
  flags under-report region-scrolls and were judged unsound, so the whole
  viewport is walked unconditionally. There is also **no per-terminal
  "content changed" short-circuit** at the widget or `render_one_terminal`
  level, so a chatty visible agent drives the full walk at ~refresh rate even
  when a handful of cells changed. (`RenderState::update`, `terminal_stack.rs:4354`,
  is a further ungated per-tile FFI snapshot on the same path.)
- **Cost scales with visible tiles, not total slots** — good: Tabs mode renders
  only the active terminal; Splits mode renders one walk per leaf tile; hidden
  slots are skipped and defer their VT feed (`terminal_stack.rs:3551,4270`).
  Scrollback depth (default 50,000 lines) does **not** enter the per-frame walk
  — render touches only the current viewport snapshot.
- **Fix:** short-circuit `render_one_terminal` when the slot's VT snapshot is
  unchanged since the last paint (a content revision/seq compare, not the
  distrusted per-row dirty flags); and/or batch the per-cell FFI into one
  row-at-a-time fetch to cut the ~5×/cell round-trips.
- **Tag:** render. **Priority: high** (dominant terminal per-frame cost; the
  "chatty agent" stall).

### R-6. Synchronous VT parse (`vt_write`) is O(bytes) on the UI thread

- **Evidence:** during drain, a visible/focused slot's bytes are fed
  synchronously — `append_output` → `slot.vt.feed(bytes)`
  (`terminal_stack.rs:2544`) → `Terminal::vt_write` FFI
  (`crates/libghostty-vt/src/terminal.rs:233`). Cost scales with byte volume
  inside the parser, on the one UI thread. Plus an unconditional O(bytes)
  OSC-52 scan per chunk (`forward_osc52`, `terminal_stack.rs:2538`), run even
  for hidden slots.
- **Already mitigated:** the drain bounds (§3), adjacent-output coalescing
  (`helpers.rs:568`), and the hidden-slot deferral (`pending_feed`,
  `terminal_stack.rs:2549-2557`) keep this from starving the keyboard, and the
  parse itself is unavoidable per byte and cannot move off-thread (§5). Listed
  for completeness, not as a new fix target beyond D-0's budget cap.
- **Tag:** other. **Priority: low.**

## 3. Drain phase — event batches & projection

Drain is 7.5 % of overruns but the worst single reading was 172 ms, and the
structure makes it unbounded.

### D-0. The drain budget bounds *receiving*, not *handling* — the core bug

- **Evidence:** in `drain_daemon_events` (`helpers.rs:518-560`) the cap check
  `collected.len() >= MAX_EVENTS_PER_TICK || start.elapsed() >= DRAIN_BUDGET`
  is at **`helpers.rs:530`, inside the `try_recv` collection loop only**. The
  actual work — `for evt in coalesce_adjacent_output(collected) {
  model.dispatch_daemon_event(evt); }` (`:546-548`) and `flush_pane_sync()`
  (`:553`) — runs **after** that loop with **no interior time or count check**.
- **Consequence:** `DRAIN_BUDGET` (8 ms) / `MAX_EVENTS_PER_TICK` (256) bound
  only how long we spend pulling events off the channel, never how long we
  spend handling them. A single oversized event (a full `Snapshot`) is
  collected in one `try_recv`, passes the cap trivially (1 event, ~0 ms), then
  dispatches with zero budget enforcement. The 172 ms reading is consistent
  with this: the cap literally cannot fire mid-dispatch.
- **Fix:** move the budget check into the dispatch loop — stop dispatching and
  carry the remainder to the next iteration once 8 ms is spent *handling*, not
  just *receiving*.
- **Tag:** drain. **Priority: high** (it is the root of the drain overruns).

### D-1. Per-event full sidebar rebuild on `WorkspaceUpserted` — not coalesced

- **Evidence:** `coalesce_adjacent_output` (`helpers.rs:568-604`) merges **only**
  adjacent `TerminalOutput`; every other variant falls through `other =>
  out.push(other)` (`:600`). `WorkspaceUpserted` → `handlers.rs:279-310`:
  deep-clones the whole `Workspace` (`:308`) then calls `recompute_visible()`
  (`:309`) → `recompute_visible_inner` (`components/sidebar/mod.rs:2386-2407`)
  → `compute_visible` (`crates/tui-core/src/inbox/mod.rs:154-303`), which
  filters, buckets by repo, and **sorts every bucket** (`inbox/mod.rs:234`) —
  **O(W log W)** with many BTreeMap/BTreeSet/Vec allocations — plus
  `recompute_stacks` → `detect_stacks` (`crates/core/src/stack.rs:56-113`),
  **O(P)** over open PRs.
- **Consequence:** a poll that changes N workspaces arrives as **N separate**
  `WorkspaceUpserted` events, handled individually → **O(N · (W log W + P))**
  in one drain, all past the D-0 budget. This is the primary drain overrun and
  is very likely the **mechanism behind #1030** (the sync-burst freeze) — the
  drain-phase counterpart to the render dominance. **Coordinate with the
  in-flight #1030 fix so this isn't addressed twice**; if #1030 lands the
  coalescing, D-1 collapses to a verification item here.
- **Fix:** coalesce a poll's `WorkspaceUpserted` batch into the map first, then
  `recompute_visible` **once** per drain (like `flush_pane_sync` already does
  for projection).
- **Tag:** drain. **Priority: high.**

### D-2. `Snapshot` rebuilds everything, twice, with an O(T²) pass

- **Evidence:** `handlers.rs:205-234`: `workspaces.clear()` + clone every
  workspace (`:210-214`, O(W)); rebuild agent/terminal maps (`:215-229`, O(T));
  `rebuild_agent_aggregates()` (`:230` → `handlers.rs:499-530`) which iterates
  all `agent_terminal_states` per key → **O(T²)**; then the full O(W log W)
  `recompute_visible`. Separately, at the model level, `events.rs:1140-1156`
  rebuilds the projects map and calls `apply_projects` → `recompute_visible`
  **again** (`mod.rs:664`). A single Snapshot can trigger the full rebuild
  **twice**.
- **Fix:** dedupe the double recompute (the projects path is *already* guarded
  by `if projects != self.projects`, `mod.rs:663`, so the second recompute only
  fires on a genuine projects change — the remaining win is folding it with the
  Snapshot handler's recompute); make `rebuild_agent_aggregates` a single pass.
- **Tag:** drain. **Priority: low** (the guard already caps the common case).

### D-3. `flush_pane_sync` unconditional large-struct clones

- **Evidence:** correctly coalesced to once per batch (guarded by
  `needs_pane_sync`, `events.rs:818-823`), but `sync_panes` (`events.rs:2649-2723`)
  deep-clones the selected `Workspace` (`:2650`), its `StackPosition` (`:2700`)
  and `SessionLayout` (`:2692`) and re-sets the right pane + terminal stack
  (`:2701-2704`) **unconditionally** — even when the net selection is identical
  to before the batch (only the `FocusWorkspace` IPC emit is deduped, `:2660`).
- **Fix:** short-circuit the clones/setters when the resolved selection key is
  unchanged.
- **Tag:** drain. **Priority: low** (bounded to once/batch).

Other per-event `recompute_visible` triggers, each O(W log W): `TerminalsRebadged`
(`handlers.rs:493`), `AgentState` on asking/limit change (`:460-462` — bursts at
detector cadence are the risk), `SessionCreated`/`SessionEnded`/`WorkspaceRemoved`.

## 4. Non-sync stall causes ("other things")

- **Terminal output floods (chatty agent).** Ties to drain + resync (§1: 178
  resyncs), R-5 (grid walk with no content-changed short-circuit) and R-6
  (O(bytes) VT parse). Coalescing of adjacent `TerminalOutput` already caps
  the event count (`helpers.rs:568-604`), and hidden terminals defer their VT
  feed (`displayed` gate, `components/terminal_stack.rs:915-926`), but a
  *visible* chatty agent still pays full VT-parse + full-viewport grid render
  per throttled frame.
- **Large PR bodies (markdown).** R-1/R-2 — the single biggest uncached
  per-frame scaler.
- **Resize / repaint.** A `Resize` forces `force_full_redraw`
  (`helpers.rs:1551-1552`); focus-regain repaints from scratch. One-shot, but
  each is a full uncached `view()`.
- **Mouse-wheel scroll.** Already mitigated — scroll redraws route through the
  background throttle rather than painting per notch
  (`helpers.rs:759-771`, `helpers.rs:1402-1416`).
- **Ticks (8.9 % of overruns).** The tick bodies themselves are cheap
  (`events.rs:2514-2635`); the tick phase over-runs almost entirely from
  suspend artifacts and from `polling_tick` → `model.update(msg)` occasionally
  dispatching real work inside the tick bracket. Minor; no dedicated fix
  beyond R-0's clock-artifact tagging.

## 5. The `!Send` single-thread constraint

`libghostty-vt` holds raw pointers and is `!Send + !Sync` by construction
(`crates/tui-term/src/session.rs:97-123`, marked with a `PhantomData<*mut ()>`;
`crates/tui/src/components/terminal_stack.rs:1142-1145`). VT parse (`vt.feed`)
and grid→ratatui render therefore **must** run on the UI thread. Everything
below competes with them for that one thread:

- daemon-event drain + coalesce + `dispatch_daemon_event` + `flush_pane_sync`,
- all three panes' widget rebuild in `view()`,
- markdown rendering (R-1),
- key/mouse dispatch,
- the tuirealm message pump.

What *can* move off-thread is already off it: the daemon owns the PTYs and
polling; a dedicated reader thread does the blocking `crossterm::event::read`
(`helpers.rs:1127-1157`); hidden terminals defer their feed. What *cannot*
move is the VT parse/render itself. The lever is therefore **not** "move VT
off-thread" (unsound) but "stop doing avoidable O(content) work on that thread
every frame" — i.e. the caching fixes R-1/R-3, and budget-capping the drain
between events (§3).

## 6. Input starvation — confirmed

The run loop is strictly single-threaded and processes **exactly one input
event per iteration**, dispatched at the *bottom* of the loop after drain,
ticks, messages, and render (`helpers.rs:1391-1418`). A keystroke that arrives
while any phase is running cannot be serviced until that phase finishes and the
loop returns to `wait_for_wake`. So **worst-case input latency = the longest
work phase** — which the telemetry puts at p90 676 ms (render), and storms
chain ~8 of those back-to-back.

The stale-input guard drops buffered key/mouse events older than 500 ms
(`STALE_INPUT_MAX_AGE`, `helpers.rs:734`, `should_drop_stale_input:742-750`) so
a recovered UI doesn't burst-fire a backlog. That means a stall long enough to
age a keystroke past 500 ms **discards** it — the user's "can't type"
experience, by design, once the loop is that far behind.

**Confirmed in real telemetry: 28 dropped-input episodes.** Excluding the one
suspend case (`dropped=558 oldest_ms=31238`), episodes show `oldest_ms` of
~900–1,200 ms with tens of events dropped (e.g. `dropped=59 oldest_ms=1169`,
`dropped=42 oldest_ms=916`, `dropped=33 oldest_ms=926`). These are **not**
suspends — they are ~1 s render/drain storms during which real keystrokes aged
out and were dropped. Input starvation during heavy phases is real and
measured, not hypothetical.

**Fix direction:** the one-event-per-iteration structure means input can only
be prioritised by making the phases it waits behind cheaper (R-1/R-3) or
budget-capping them (§3). A stronger fix is to **service pending input between
sub-steps of an expensive phase** — e.g. check `input_rx` mid-drain and
mid-render-batch — so a keystroke pre-empts the tail of a burst instead of
waiting for it to finish. That is the only structural cure for the "can't type
during a flood" report while VT stays on-thread.

## Prioritised fix backlog (candidate follow-up issues)

| # | Cause | Tag | Priority | Fix |
|---|-------|-----|----------|-----|
| D-0 | Drain budget bounds *receiving*, not *handling* — one big event blows past 8 ms | drain | **high** | Move the budget check into the dispatch loop; carry remainder |
| D-1 | Per-event full sidebar rebuild on `WorkspaceUpserted`, not coalesced → O(N·W log W)/poll | drain | **high** | Coalesce a poll's upserts, then `recompute_visible` once/drain |
| R-1 | PR/issue body markdown re-rendered ×2/frame, uncached | render | **high** | Memoise body render keyed by `(body_rev, width)`, size from cache |
| R-3a | Sidebar per-row badge scan O(rows×terminals) | render | **high** | Precompute `SessionKey → badges` map once/frame |
| R-5 | Terminal full-viewport grid walk, ~5 FFI/cell, no content-changed gate | render | **high** | Short-circuit unchanged VT snapshot; batch per-cell FFI |
| 6 | Input starvation during heavy phases | other | **high** | Service pending input mid-drain/mid-render-batch |
| D-2 | `Snapshot` rebuilds everything twice + O(T²) aggregate (projects path already guarded) | drain | low | Dedupe residual recompute; single-pass aggregate |
| R-2 | `body_wants_rich_modal` full-body scan/frame | render | medium | Fold into R-1 memo |
| R-3b/c | Sidebar attention counters + per-row rebuild | render | medium | Single-pass counters; cache per-row `Line`s |
| D-3 | `flush_pane_sync` clones on unchanged selection | drain | low | Short-circuit when selection key unchanged |
| R-4 | Reader modal clones doc/frame | render | low | Render cached lines by ref/windowed |
| R-6 | Synchronous O(bytes) VT parse on UI thread | other | low | Already mitigated; covered by D-0 budget cap |
| R-0 | Multi-second "render" = suspend/blocked-write mis-tagged | render (reporting) | low | Separate wall-clock from CPU; tag artifacts |
| R-3d | Misc sidebar per-frame allocations | render | low | Cache with the projection |
