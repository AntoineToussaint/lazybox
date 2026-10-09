# Terminal scrolling — the canonical model (#371)

Terminal scrolling regressed over and over (#306, #321, #360, #42,
#362). Each point-fix closed and the bug came back in a new guise. This
document is the root-cause writeup the recurrence never had, and it
describes the structure that now makes a silent regression a compile- or
test-time failure instead of a shipped bug.

Everything here lives in **`crates/tui/src/components/terminal_stack.rs`**
unless noted. The regression harness is
**`crates/tui/tests/terminal_scroll.rs`**.

## The model, end to end

```
daemon PTY ─▶ Event::TerminalOutput / Snapshot.replay (raw bytes over IPC)
           │
           ▼
TerminalStack::append_output / on_event(Snapshot)
           │  (focused/visible → feed now; hidden → stash in pending_feed)
           ▼
TerminalSlot.vt : TerminalVt   (one libghostty-vt parser per terminal)
           │  vt.feed(bytes) → vt_write → grid + scrollback pin
           ▼
      viewport pin  ── the ONLY scroll state. There is no lazybox-side
                       offset field; the position lives inside libghostty
                       and is read back on demand via `scrollbar()`
                       ({total, offset, len}).
```

Key consequences:

- **libghostty owns the offset.** lazybox never caches a scroll offset;
  it reads `scrollbar()` when it needs to render the gutter or report an
  outcome. There is exactly one viewport per terminal.
- **Rendering follows the pin.** `GhosttyTerminal`
  (`crates/tui-term`) walks whatever rows the current viewport exposes.
  It holds no scroll state.
- **Fresh spawn and reattach share one init.** Both `TerminalSpawned`
  and `Snapshot` build the slot through `make_slot` (a fresh
  `TerminalVt`); reattach just seeds `pending_feed` with the daemon ring
  replay, which flushes into the same parser on first render. The
  `fresh_and_reattach_reach_identical_scroll_state` test pins that they
  cannot diverge.

## The single owner (the #42 promise, kept)

There is **one** function that mutates a viewport:

```rust
impl TerminalVt {
    fn scroll(&mut self, request: ScrollRequest) -> ScrollOutcome
}
```

`ScrollRequest` is the entire vocabulary — `By(delta)`, `Top`, `Bottom`,
and `ToRow(row)`. `scroll` is the only caller of libghostty's
`scroll_viewport` in the whole TUI; `scroll_viewport_has_a_single_owner`
reads the source and fails the build if a second call appears anywhere
else. No handler pokes a raw offset.

`ToRow` is the only **absolute** verb and the only one no gesture
produces: it exists for `restore_anchor` (below), which has to re-place
the pin on a grid that was just rebuilt underneath it. It maps to
libghostty's `GHOSTTY_SCROLL_VIEWPORT_ROW`, whose row space is the one
`Scrollbar.offset` reports — so a position read off the VT round-trips
back into it unchanged. Until #1909 the Rust binding exposed only
TOP/BOTTOM/DELTA, so the client could *read* an absolute viewport
position and not *write* one, and every restore was re-derived as a
delta from wherever the viewport happened to be.

Every surface funnels through it:

| Surface | Entry point | Targets |
|---|---|---|
| Mouse wheel | `scroll_terminal(id, ±3)` on `terminal_at(col, row)` | tile **under the cursor** |
| `Shift-PageUp/PageDown` | `scroll_active(±8)` | focused tile |
| `Shift-Home` / `Shift-End` | `scroll_to_top` / `scroll_to_bottom` | focused tile |

`scroll_terminal(id, delta)` (cursor-directed, by id) and `scroll_active`
(focus-directed) both call `TerminalVt::scroll`, and `scroll_to_top` /
`scroll_to_bottom` call it with `Top` / `Bottom`. One choke point, one
owner.

### A no-op can never be silent

`scroll` reads the scrollbar both before and after every request that
should move and always returns a typed `ScrollOutcome`:

- `Moved { from, offset, total, len }` — the viewport demonstrably
  moved; `from != offset` is guaranteed by the owner.
- `NoScrollback` — `total <= len`: there is nothing to scroll into.
- `AtBoundary { boundary, ... }` — the viewport was already at the top
  or live bottom requested.
- `Noop` — an explicit `By(0)` request.
- `Stalled { request, ... }` — scrollback exists, the viewport is away
  from the requested boundary, but the post-request offset did not
  change. This is the typed regression signal for a broken VT scroll.
- `StateUnavailable` — libghostty could not provide a scrollbar state.
- `NoTerminal` — no terminal resolved.

This is why "no history yet" is no longer indistinguishable from "the
Delta path broke" — the recurring confusion behind #306/#321/#360. The
harness asserts each `Moved` outcome's `from` and `offset` against the
actual viewport, separately pins boundary/no-op outcomes, and unit-tests
that an unchanged mid-buffer transition is `Stalled`, never a fake move.

## Per-tile targeting (#362)

In a split layout the wheel used to scroll the *focused* tile no matter
which tile the pointer was over. The wheel now resolves the tile under
the cursor (landed on `main` as #377, this effort absorbs it):

- Each tile's on-screen rect is **recorded during render** (`tile_hits`);
  `terminal_at(col, row)` hit-tests the wheel event against them and
  returns the terminal the pointer is over, or `None` over pane chrome (a
  tab strip, a divider, the accent seam) — where the wheel falls back to
  the focused tile. Recording the real rendered rects avoids re-deriving
  the split geometry and can't drift from what was drawn.
- The wheel handler calls `scroll_terminal(id, delta)` for that terminal.
  Screen mode and mouse tracking never redirect the gesture into the app.
- The keyboard path stays focus-directed — it has no pointer.

The scroll *mutation* for every one of those still funnels through the
single owner (`TerminalVt::scroll`), so per-tile targeting and the
no-silent-no-op guarantee compose rather than fight.

## The other mutator: writes (#1909)

The single owner above makes *scrolling* total and observable. It says
nothing about what a **write** does to the pin it just moved — and a write
is the other mutation of the viewport's meaning, because it changes the
content the pin points into. `scroll()` moved the pin, `feed()` mutated
what it pointed at, and the two shared no contract: no parked state in the
type, nothing re-asserted after a write. That gap is why scrollback
corruption kept coming back through #909, #1547, #1548, #1550 and #1554 —
each of those treated it as a painting problem, and the paint path is
sound (the widget distrusts libghostty's dirty flags and walks every row
every frame, #239). Repaint correctness cannot fix an anchor nobody owns.

`TerminalVt` now owns the pin's meaning across content mutation:

```rust
enum ViewportAnchor { Bottom, Parked { rows_above_bottom: u64 } }
```

- **Rows above the live bottom, not an absolute row.** An absolute row
  names a position in one particular grid; a rebuild produces a grid whose
  history above the tail has a different depth, so the same integer would
  name different content. Distance from the tail survives a rebuild that
  only changes what is *above* the tail — and holding that invariant is a
  responsibility, not an assumption (see the straddle refusal below).
- **`scroll()` sets it.** A move there is the user choosing a place to
  read.
- **`feed()` re-derives it.** Not re-imposes: libghostty holds a real
  *content* pin and compensates across appends, so a viewport parked 7
  rows up is 10 rows up after 3 lines arrive, still on the same rows. The
  anchor follows that. Forcing the old 7 back would drag the user down the
  buffer on every chunk. (Verified on #1909: park mid-scrollback, feed, and
  the visible rows are byte-identical. The "ring eviction renumbers the
  pin" theory was tested and refuted.)
- **`restore_anchor()` re-places it**, absolutely, through the owner's
  `ToRow` verb, computed against the new grid's own extent.

### Where the grid is replaced, and what that does to the pin

`TerminalSlot::rebuild_grid` is the one place a grid is thrown away, and
its `GridPin` argument forces each site to *state* its policy instead of
inheriting whatever a fresh parser does (which is: start at the bottom).

- **Deep-scrollback capture adoption** (`apply_scrollback`) — `Keep`. The
  user is mid-scroll; that is what triggered the fetch. The rebuild
  deepens history above them and leaves the tail alone.
- **Resync after dropped output** (`resync_terminal`) — `LiveBottom`. The
  ring replay is a bounded tail of dropped output and may hold *less*
  history than the grid it replaces, so no row in it reliably means "where
  the user was". Returning to the tail is a decision here, not an
  accident.
- **A rebuild that fails** (libghostty allocation failure) leaves the grid
  *and* the pin alone — degrading to the last coherent grid includes the
  viewport the user was reading.
- **Fresh spawn / reattach** — `make_slot`; the viewport starts at the
  bottom, identical init for both (see above). **Hidden-buffer flush**
  (`flush_pending`) is an ordinary `feed`, so the anchor is re-derived.
- **`\x1b[3J`** (erase-scrollback) from the inner program legitimately
  empties scrollback → the next scroll reports `NoScrollback`. Not a bug.

### Why a straddling batch refuses the capture

`GridPin::Keep` is sound only while the rebuild leaves the live tail
alone, because that tail is what the anchor is measured from. A delivered
`TerminalOutput` is a **run** of chunks — the client coalesces adjacent
output (`realm/model/helpers.rs::coalesce_adjacent_output`) and keeps only
the run's `first_seq..=seq`, so the byte offset where one chunk inside it
ends is unrecoverable. A retained batch is therefore wholly covered by the
capture's watermark, wholly uncovered, or straddling it and unsplittable.

Re-feeding a straddling batch whole re-draws rows the capture already
holds. That is #1909's reported symptom twice over: the duplicated block
(the second copy continuing on the capture's unterminated last row, so it
reads as "truncated at the same word"), *and* a parked viewport dragged
down by exactly the duplicated row count, because the tail the anchor
measures from just grew. So a straddling batch refuses the capture, like a
hole in the retained stream does: the local grid holds every byte and is
only shallower, `scrollback_stale` is still set, and the next upward
scroll re-captures. Declining costs depth for one visit; re-feeding costs
correctness every time.

### Asserting the distance is not asserting the pin

The pre-#1909 regression test checked that the viewport's
distance-from-bottom survived a capture adoption. It did — while the
content at that distance moved. A viewport assertion compares the rendered
rows (`viewport_rows` in the test module, read through the same render
state the widget walks), not `grid_text` and not scrollbar arithmetic.

## Wheel ownership

The wheel always belongs to lazybox's terminal history. Screen mode and mouse
tracking affect rendering and clicks, not scrolling. The tmux backend rejects
alternate-screen requests at the pane boundary, and Claude launches with
`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`; the latter selects Claude's inline
renderer instead of its bounded full-screen repaint loop so the conversation
actually flows into retained pane history. This is a PTY correctness override,
so a colliding per-repository environment value cannot disable it. The attach
client also stays on the primary screen so the same output accumulates in
libghostty scrollback. An upward wheel can fetch the backend's deeper
`capture-pane -J` history; tmux joins soft-wrapped screen rows before replay so
display wrapping does not become hard line breaks. The wheel never writes SGR
mouse reports or synthesized keys into the inner program.

A daemon cannot replace the inherited environment of a Claude process that
survived an upgrade. PTY launch generations are persisted with new sessions;
when recovery finds an older generation, each client receives a persistent
notice to close and reopen that terminal. The session stays attached until the
user chooses to restart it, so an upgrade never kills in-flight agent work.
Persisted generations newer than the running daemon are treated as compatible,
so temporarily downgrading lazybox does not falsely condemn a newer session.
The notice is derived from the exact terminal snapshot sent on subscribe and
stays outside provider polling, so concurrent teardown cannot produce a stale
warning or turn terminal lifecycle state into a provider-sync failure.

## Retention depth and repaint churn (#857)

Scrollback is finite on both sides of the conduit and the two are kept in
lockstep:

- The tmux backend retains `history-limit` lines per pane
  (`crates/server/src/backend/tmux.rs`), and the deep-scrollback fetch
  captures from `-S -{history-limit}`, so the fetch can never under-read
  what tmux was told to keep.
- Each client VT takes that same line count as its
  `max_scrollback_lines` (`CLIENT_SCROLLBACK_LINES` in
  `terminal_stack.rs`). libghostty now exposes a **real line limit**
  (`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES`); the older binding bounded
  scrollback by bytes of page memory despite the C header's "number of
  lines" wording, which is why `client_scrollback_bytes` still sets an
  explicit byte backstop — a fresh terminal carries a modest *default*
  byte limit that would otherwise bind long before a large line count is
  reached. Both are set; whichever is hit first prunes. If the cap were
  shallower than tmux's, a deep fetch would replay the full history into
  the VT and the parser would silently drop everything past the cap — the
  deeper tmux history would never become scrollable. (The old flat
  `10_000` was ~10 KB, only a few hundred lines — a client-side bottleneck
  in its own right, #857.)

  The line limit prunes at **page granularity**, so retention is an
  estimate in one direction only: usually somewhat *higher* than asked
  for, and it cannot go below one page. A small cap therefore reads far
  above its own number, and that is the floor rather than an unenforced
  cap. Measured on this tree at 120 cols (#1909): asking for 100 lines
  and feeding 3 000 retains 229 rows, 30 000 retains 301, 120 000 retains
  409. At production depth the limit is what binds —
  `max_scrollback_lines: 10_000` fed 60 000 lines retains 9 709 — so
  client VT history is bounded by the line limit, not only by the byte
  backstop. `crates/libghostty-vt/tests/scrollback_limits.rs` pins both
  directions, including
  `a_shallow_line_limit_prunes_regardless_of_the_byte_ceiling`.

Both come from **`terminal.scrollback_lines`** (default 50000). Raising
it keeps more of a long session at the cost of per-pane RAM on the tmux
server *and* in every client VT (roughly linear in the line count).

**Why the depth matters more here than for a normal shell.** With
alternate-screen forced off (so agent output flows into retained history
at all — see *Wheel ownership*), a full-screen TUI that redraws its
viewport spills every repaint into history, burning the budget far faster
than genuine new content. Claude Code is mitigated at the source: it
launches with `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, which selects its
*inline* renderer (append-mostly) instead of a full-frame repaint loop, so
its churn is a fraction of a naive full-screen redraw. The raised,
configurable limit is the second half of the mitigation: it widens the
window before eviction bites for a genuinely long session. Once history
does exceed the limit the oldest lines are evicted for good — no client
fetch can recover them — so the limit is the documented hard bound on how
far back any session can scroll.

## The regression harness

`crates/tui/tests/terminal_scroll.rs` drives every surface through the
real entry points:

- Fresh-spawned agent — wheel, `Shift-PageUp/PageDown/Home/End`.
- Reattached session (Snapshot replay).
- Fresh and reattach reach identical scroll state.
- Split tiles — scrolling a non-focused tile leaves the focused one put
  (#362); the keyboard scrolls the focused tile.
- Empty local history vs. populated history.
- No silent no-op: actual before/after offsets for movement, typed
  boundary/empty/no-op reasons, and a `Stalled` result for an unexpected
  unchanged offset.
- The single-owner source guard scans every Rust file in the TUI crate
  (and brace-matches the owner's body), so a raw viewport mutation added
  in another module fails the harness.

The full end-to-end wheel routing for #362 (which tile a real
`handle_mouse` scrolls, including the SGR/arrow forward) is covered in
`crates/tui/src/realm/model/tests.rs`; this harness owns the seams below
that.

The bar: a change that breaks any scroll surface turns a test red. If a
"scrolling broken again" report ever appears, it points at a **missing
surface in this harness** — add the case, then fix the code.
