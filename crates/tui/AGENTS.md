# The TUI library

A tuirealm-based renderer over IPC. `tui` is a **library**; the `lazybox`
binary lives in `tui-boot`, which carries the daemon, provider and store
wiring this crate is not allowed to touch.

Read [`AGENTS.md`](../../AGENTS.md) first; this file only adds client depth.
Key chords and the action catalog are next door in
[`../tui-core/AGENTS.md`](../tui-core/AGENTS.md).

## The dependency boundary is a compile error

`tui` may depend only on `{ipc, tui-core, tui-term, config, core}`. A
`use lazybox_store::…` here does not compile, and `crates/core/tests/dep_rules.rs`
fails on a new edge even before that. If a pane needs data it cannot reach,
the answer is an IPC event, not a dependency — the client is a renderer, and
state that lives here is state the daemon cannot recover after a reconnect.

## Structure

- **`Model`** (`src/realm/model/`) is the orchestrator: the three panes as
  typed fields, focus (`PaneFocus`), keyboard and mouse dispatch, daemon-event
  fan-out, the IPC client. Split into `keys.rs` / `events.rs` / `dispatch.rs` /
  `modals.rs` / `helpers.rs`.
- **Panes** — the domain structs `Sidebar`, `RightPane`, `TerminalStack` live
  in `src/components/`; thin tuirealm wrappers in `src/realm/components/`
  delegate render and key dispatch to their inherent methods. Put logic in the
  domain struct, where it can be tested without a realm.
- **Modals** are `AppComponent`s mounted on `Model::modal_stack`.
- **Two picker shapes, and only two.** `Choice<T>` (`realm/components/choice.rs`)
  is the single- or multi-select with axis sections and typed payloads; opt in to
  filter-as-you-type with `.with_search(|item, query| …)`, which hands the
  matching to the item's own type rather than re-deriving it from the rendered
  label. `FilterableList` (`realm/components/filterable.rs`) owns the
  single-column filter-first pickers (jump, prompt history, snippets). A long
  list needs one of these — not a third copy of the key protocol.
- **Setup wizard** (`src/setup_flow.rs`) is a realm-native `SetupRunner` state
  machine driving Choice / Loading / Error modals.
- Migration notes for the tuirealm port: `src/realm/MIGRATION.md`.

## The VT mirrors the PTY, never the pane

One `libghostty-vt` instance per terminal slot in `TerminalStack`; it is
`!Send` and lives on the UI thread. It is sized **only** from the size stamps
the daemon puts on output chunks, resize announcements and replay spans. The
pane drives `Command::Resize` and nothing else.

Sizing the VT from the widget looks obviously correct and is the bug: bytes
laid out at one size reparsed at another duplicate lines in scrollback. Three
separate client-side fixes recurred before the size authority moved to the
daemon; do not reintroduce a local guess.

## The viewport pin has one owner across writes, not just across scrolls

`TerminalVt::scroll` is the single owner of viewport *movement* (#42/#371) and
it stays that way: nothing outside it calls libghostty's `scroll_viewport`, and
`tests/terminal_scroll.rs::scroll_viewport_has_a_single_owner` brace-matches its
body and fails the build on an escapee. A new verb added *inside* the owner is
fine; a call anywhere else is not.

That owner was only ever half the problem. `scroll` moves the pin; `feed`
mutates the content the pin points into; before #1909 neither said anything
about the other, so a parked viewport had no owner across writes. The pairing
is now explicit:

- `TerminalVt::anchor` holds the pin's *meaning* — `Bottom`, or parked N rows
  above the live bottom. Rows-above-bottom, not an absolute row, because a
  rebuild changes how deep the history above the tail is and an absolute row
  would then name different content.
- `feed` **re-derives** the anchor after a write **while parked**. It does not
  re-impose it: libghostty keeps a real content pin across appends, so three
  new lines under a viewport parked 7 rows up correctly leave the user on the
  same rows, now 10 above the bottom. Forcing 7 back would drag them down the
  buffer per chunk.
- A write while **following the tail** re-derives nothing, and costs no FFI
  read at all (#1918). Appending cannot un-park a viewport already at the live
  bottom, so the cached `Bottom` is already what a read would return — and the
  read is `scrollbar()`, expensive by its own contract, on the hottest path in
  the client. Entering the parked state goes through `scroll`, which reads
  anyway, so no transition is missed. The equivalence is held by
  `following_a_tail_is_equivalent_to_re_deriving` against escape-heavy traffic
  (region scrolls, an alt-screen round trip, a hard reset) rather than left as
  an argument; see the hot-path budgets below.
- `TerminalSlot::rebuild_grid` is the one place a grid is replaced wholesale,
  and its `GridPin` argument makes each site state what happens to the pin:
  `Keep` re-asserts the anchor (the capture adoption, which only deepens
  history above the tail), `LiveBottom` returns to the tail on purpose (the
  ring resync, whose replay may be shallower than what it replaces). A rebuild
  that fails leaves both the grid and the pin alone.
- `restore_anchor` places the pin **absolutely**, via `ScrollRequest::ToRow`
  over `libghostty_vt::ScrollViewport::Row`. That ABI verb shares its row space
  with `Scrollbar.offset`, so a position read off the VT is written back
  unchanged. The previous delta restore worked only because a fresh grid starts
  at the bottom — every restore was a guess re-derived from a distance.

Asserting the *distance* is not asserting the pin: the pre-#1909 test checked
that the distance survived a rebuild, and it did, while the content at that
distance moved. A viewport assertion compares the rendered rows
(`viewport_rows`), not `grid_text` and not a scrollbar arithmetic.

## The hot paths have budgets, and the budgets are counted

Four paths in this crate run often enough that what they are *allowed to
do per call* is part of their contract. The gate is
`tests/terminal_hot_paths.rs`; the wall-clock numbers are `make bench`.

| Path | Budget per call | Counted by |
| --- | --- | --- |
| `TerminalVt::feed` — every output chunk of every terminal | **0** scrollbar derivations while following the tail; **1** while parked | `a_following_tail_feed_reads_no_scrollbar`, `a_parked_feed_still_re_derives_once_per_write` |
| `TerminalVt::scroll` — a user action | a **constant** (2: `before` and `after`), never a function of depth or distance | `a_scroll_reads_a_constant_number_of_scrollbars` |
| `TerminalStack::render` — per visible tile per frame | **1** scrollbar derivation; **≤ rows + 1** row fetches on a painted frame and **0** on a repaint of an unmutated grid | `a_painted_frame_reads_one_scrollbar_per_tile`, `a_frame_walks_each_viewport_row_at_most_once`, `an_unchanged_frame_walks_no_rows` |
| `TerminalStack::handle_key` — every keystroke | exactly **1** `Command::Write`, **0** VT work, bounded bookkeeping | `one_keystroke_is_one_write_and_no_vt_work`, `typing_a_word_is_one_write_per_key` |

**`libghostty_vt::Terminal::scrollbar()` is expensive by contract and
cheap in this build — respect the contract anyway.** Its doc says
"arbitrary pins are expensive … not too frequently". Measured
(`make bench-cpu`), it is **~5 ns of CPU at any viewport depth** — the
same parked 9000 rows up a 9860-row grid as at the live bottom. So #1910
putting it on `feed` cost ~0.5% of a `vt_write` (~960 ns) and was *not*
what the user felt in #1918; the box was at load average 75.

Which is why the budget is still zero on the following-tail path. The
number above describes today's libghostty, not the API's promise: the
warning is the implementation's licence to become expensive, and a hot
path built on "it happens to be 5 ns" breaks silently the day it isn't.
Keep the call off per-chunk paths on contract grounds, and do not add
caching or complexity to shave it — that trade was measured and refused
once already (see `TerminalStack::render`'s comment on not caching the
reading across frames).

**Gate on work counted, not on time.** A wall-clock threshold cannot be
the gate here. The suite already has a loaded profile (`make test-loaded`)
because fixed timeouts flake on a shared box, and the regression above was
reported at load average 75. A time budget is then either too loose to
catch anything or tight enough to fail PRs that changed nothing. A count
is exact on every machine. Timing numbers are for humans, in the PR body.

**Every budget assertion carries a positive control.** The dangerous
failure of a counting gate is not a wrong number, it is a counter that
stopped observing — then every "this does no work" assertion passes green
while the regression ships. `vt_budget::Counts::assert_live` is how each
test proves the instrument is alive before trusting its zero. That is also
why the counters in `lazybox_tui_term::vt_budget` are compiled in
unconditionally rather than behind a cargo feature: a feature left off
makes the whole gate vacuous. They are per-scrollbar-read and per-**row**
(never per cell) for exactly that affordability.

**The counting shim cannot be bypassed.** `TerminalVt::scrollbar` is the
one place this crate reaches the binding, and
`scrollbar_reads_have_a_single_counted_owner` brace-matches its body and
fails the build on a raw `.terminal.scrollbar(` anywhere else in `src/` —
the same mechanical backstop `scroll_viewport` has carried since #371. A
new read goes through the owner, or the budget stops seeing it.

**The every-row walk in `ghostty_widget.rs` is justified, not a defect.**
libghostty's dirty flags are unsound as a redraw-skip signal against a
viewport-indexed cache (#239, and the module docs there explain it), so
the widget walks every cell of every row. What bounds it is the
content-revision gate in `TerminalStack::render`: `(content_rev, rect)`
logs the VT's *inputs*, so an unchanged grid blits a cached frame and
walks nothing. Do not "optimize" the walk by consulting the flags; do keep
the revision gate sound, and invalidate `last_frame_rev` **and**
`last_frame_bar` wherever the parser is replaced — a fresh parser restarts
the counter.

**Nothing on the tick path may read the config.** `Config::load()` is a
file read plus a YAML parse; every TUI caller is an action or a modal
mount. `the_tick_phase_does_not_load_the_config` discovers the `tick_*`
family from `run_loop_step` itself and audits each body, so a tick added
later is covered without anyone remembering to.

## A capture never replaces output it predates

`apply_scrollback` swaps the whole grid for the daemon's tmux capture, and
`last_seq` never rewinds — so a batch that arrived while the fetch was in
flight and is then dropped is gone for good, with nothing on screen to say so.
Every batch delivered since the fetch was armed is retained on the slot and
the part above the reply's watermark is re-fed on top of the rebuild. When
that retained stream can no longer be spliced on — it outran its cap, or a
ring resync rebuilt the grid from another baseline — the capture is *refused*:
the local grid holds every byte, it is only shallower, and the next upward
scroll re-captures.

A delivered batch is a **run** of chunks: the client coalesces adjacent output
(`realm/model/helpers.rs::coalesce_adjacent_output`) and keeps only the run's
`first_seq..=seq`, so the byte offset where any one chunk inside it ends is
gone. A batch is therefore wholly covered by the capture, wholly uncovered, or
straddling its watermark and **unsplittable** — and re-feeding a straddling
batch whole re-draws the rows the capture already holds. That was #1909: the
same block twice, the second copy continuing on the capture's unterminated last
row (hence "truncated at the same word"), and the parked viewport dragged down
by the duplicated row count because the anchor is measured from a tail that just
grew. A straddling batch refuses the capture, like a hole does.

## Which button a Confirm defaults to is a per-site decision

`Confirm::new` and `destructive()` both leave `Enter` on Yes — the chord that
raised the prompt is the intent, and `destructive()` conveys the danger with a
warning border and `⚠` title rather than by moving the default.
`default_no()` is the one builder that moves it, keeping that coloring: use it
where a stray keystroke must not fire the action — an unsolicited kill of a
running agent, a bulk wipe, an out-of-order merge. It used to be an alias for
`destructive()`, so every call site believing itself guarded was
Enter-to-confirm (#1899).

The three combinations are named: `ConfirmStyle::{Benign, Destructive, Guarded}`,
built with `Confirm::styled`. Two booleans admitted a fourth that the component
cannot represent, and `PendingRemovalRisk` — whose job is to rebuild a prompt
faithfully when `apply_removal_risks` appends the daemon's risk list — could hold
it. That re-mount has to reproduce chrome *and* default; deriving the default
from the destructive flag alone silently traded the guard back for Yes.

## A guard moves the default, so the default must not decide

A prompt defaults to No because the user may not be reading it — so No there
cannot commit anything. The workspace-removal prompt is guarded whenever the row
has a live terminal, and answering a guarded one defers (the silence `Esc`
produces, so the daemon re-prompts) instead of sending `KeepMergedWorkspace`,
which persists `CleanupPrompt::Declined`, suppresses the prompt permanently
across restarts, and has no UI to see or undo. Swapping a one-keystroke deletion
for a one-keystroke permanent retirement is not a fix.

The guard bit is decided at mount and carried on `ModalFlow::RemovalPrompt`, not
re-derived when the answer lands: an agent that exits while the modal is up must
not turn the rendered guard back into a deciding prompt. It is also ORed with the
client's live terminal count, because the daemon's `active_terminal_count` is a
snapshot from emit time and `removal_already_pending` drops the re-emit that
would refresh it.

## Markdown is hand-rolled

`components/comment_render.rs` and `right_pane/markdown.rs` do inline-noise
stripping and teaser extraction with no markdown crate. Reaching for one is a
dependency and a behaviour change, not a cleanup.

## Tests

Visually complex components carry insta snapshots; the rest use ratatui
`TestBackend`. Keys are tested through `dispatch_event` across pane focus,
because "the key does nothing" is usually resolution or availability, not
dispatch. `tests/keymap_docs.rs` regenerates the published keybinding
reference and fails on drift — see
[`../tui-core/AGENTS.md`](../tui-core/AGENTS.md).

Theme state is process-global: a test that sets it must account for the
per-binary sandbox (`mod common;`) other tests in the crate share.
