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

`ScrollRequest` is the entire vocabulary — `By(delta)`, `Top`, `Bottom`.
`scroll` is the only caller of libghostty's `scroll_viewport` in the
whole TUI; `scroll_viewport_has_a_single_owner` reads the source and
fails the build if a second call appears anywhere else. No handler pokes
a raw offset.

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

## Where scroll state is initialised, mutated, or can (legitimately) reset

- **Fresh spawn / reattach** — `make_slot`; viewport starts pinned at the
  bottom. Identical init (see above).
- **Resync after dropped output** (`resync_terminal`) and **hidden-buffer
  overflow** (`flush_pending`) rebuild the parser from the daemon ring;
  the viewport resets to the bottom. This is correct: the byte stream is
  being reconstructed, and the ring is authoritative.
- **`\x1b[3J`** (erase-scrollback) from the inner program legitimately
  empties scrollback → the next scroll reports `NoScrollback`. Not a bug.

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
