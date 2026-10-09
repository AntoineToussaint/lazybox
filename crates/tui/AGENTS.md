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

## A tile divider is a ratio in the session, not a percentage in config

Runners in the stack are tabs *by default* (`SessionLayout::Tabs`), but
`SessionLayout::Splits { tree, focused }` over an N-way
`lazybox_core::TileTree` is equally real and is what `lazybox log` actually
produces: `auto_split_on_spawn` defaults to `Split`, so an agent that runs
`cargo test 2>&1 | lazybox log` gets its log window as a *tile* beside it.

Each split node carries its own `ratio`, and that ratio **is** the persisted
divider position — there is no `ui.`-level percentage for it, unlike
`sidebar_pct` / `right_top_pct`. It travels with the rest of the layout
through `Command::SetSessionLayout`, which is why each workspace remembers
its own divider rather than sharing one global number. Reaching for a config
knob here would add a second, coarser source of truth for something the tree
already stores.

Three rules hold that together:

- **One door onto the ratio.** `TileTree::set_ratio_at` clamps to
  `TILE_RATIO_MIN..=TILE_RATIO_MAX`, and the pointer, the keyboard nudge and
  a restored layout all go through it. A divider dragged to the edge would
  otherwise leave a zero-width tile that still owns a PTY and still takes
  keystrokes — invisible but live.
- **The divider moves the way the arrow points.** `resize_toward` takes its
  sign from the direction alone, never from which child the focused tile sits
  in. The tempting alternative ("the current tile grows") disagrees for a
  tile in the second child, which would make `]]Shift-Right` mean two
  different things depending on which side the user had clicked into, and
  would stop it matching the mouse drag. Keyboard and pointer must not
  disagree about which way is which.
- **Hit-testing reads the frame, not a recomputation.** `render_tile_tree`
  records each divider it paints (`divider_hits`: path, line, container,
  axis) and the hit-test and drag read that back, the same way `TerminalHit`
  records its rects. The grab zone is ±1 cell — the 3-cell zone the pane
  splitters use — because a 1-cell line is not a target a pointer hits
  reliably, and one that needs a pixel-perfect aim reads as not draggable.

A drag persists **once**, on mouse-up. One `Command::SetSessionLayout` per
pointer motion would put the daemon's workspace writer on the mouse.

## A log window takes no typed input

A `LogTail` runner is `tail -F <path>` (`crates/server/src/spawn_plan.rs`) —
a process that never reads its stdin. `TerminalStack::handle_key` therefore
refuses to produce PTY-bound bytes for one at all, rather than filtering for
printables: there is no keystroke `tail -F` has a use for, so a byte that got
through would be one with no reader. Before #1920 every keystroke in a log
window became a `Command::Write`, and `Enter` additionally shipped
`TerminalInputIntent::Submit` — which `lazybox_ipc` documents as
"authoritative evidence that a turn may start" and which arms
`submission_in_flight` on that runner's activity entry.

The guard is on the **runner kind, not the pane**: the agent in the same
tiled session still types and still records. Muting the pane would pass every
"no write" assertion and break the feature.

What a log window can still do is unaffected *by construction*, not by a
carve-out — none of it reaches the refusal. Scrollback keys resolve at the
top of `handle_key`, the wheel and text selection are mouse paths in the
Model, and search is a `Section::Sidebar` action. Keep it that way: a guard
moved earlier in `handle_key` would silently take the scrollback with it,
which is what `a_log_window_still_scrolls` exists to catch.

Refusing in silence would be its own bug — "I typed and nothing happened" —
so a *typing* attempt (a printable or `Enter`, modulo SHIFT) leaves a notice
on `input_refusal` for the Model to flash. An arrow or a stray `Ctrl-C` is
refused quietly; a footer line per keystroke is noise. The daemon already
refuses the **snippet** path into one of these and calls it "a read-only log
terminal" (`spawn_handler.rs`); this is the same knowledge on the typing
path, at the boundary where the runner kind is known.

## Which button a Confirm defaults to is the user's config, on two axes

A destructive prompt's `Enter` side is not a per-site decision and not a
judgement about how scary its copy is — every one of them wears the warning
border and `⚠` title either way. It is one question: **was there a keystroke
behind this prompt?** That is the axis, and `ui.confirm_default` keys off it:

- `destructive_shortcut` (ships `yes`) — the user pressed a chord. The chord
  *is* the intent, so `Enter` completes it. `x x` archive and the rest of the
  catalog family, `g m`'s out-of-order override (both mount paths), `c` in the
  Error Inbox, the spawn key onto a claimed task, a snippet apply that would
  overwrite, the worktree-recreate confirm.
- `event` (ships `no`) — the daemon pushed it with nothing behind it. One
  prompt is on this axis: workspace removal over a row whose agent is live.

`Confirm::from_source(question, ConfirmSource::{Shortcut,Event}, defaults)` is
**the** entry point, and `ConfirmStyle::destructive_on` is the resolver it and
`PendingRemovalRisk` share. Do not add a new `default_no()` call site: put the
prompt on an axis. #1900 is what this replaces — it moved all eight destructive
prompts to No while fixing one of them, because the policy lived at eight mounts
and `ui.confirm_default` was parsed and read by nobody, so there was no smaller
lever to pull (#1899, #1921).

`default_no()` survives as the deliberate opt-out, for a prompt whose Yes loses
something no re-clone brings back *and* whose chord did not ask for that loss:
the bulk worktree wipe, the inspector's dirty-worktree delete, the rescope
sweep's "delete these workspaces" (the chord is the wizard's Finish — "save my
filter"), and the sandbox wizard's auto-connect step, where No is the
recommended answer rather than a guard. Each says so where it mounts, and each
has a test that sets the knob to `yes` explicitly so it cannot go vacuous if
the shipped default moves. A benign gate is on no axis and always affirms;
`ConfirmStyle::Benign` is for those.

The three resolved combinations stay named — `ConfirmStyle::{Benign,
Destructive, Guarded}`, built with `Confirm::styled`. Two booleans admitted a
fourth that the component cannot represent, and `PendingRemovalRisk` — whose job
is to rebuild a prompt faithfully when `apply_removal_risks` appends the
daemon's risk list — could hold it. That re-mount has to reproduce chrome *and*
default, and it reproduces the **resolved** style rather than re-deriving one:
re-resolving would be correct today and wrong the moment the resolution depends
on anything that can change while the modal is up.

## A guard moves the default, so the default must not decide

A prompt defaults to No because the user may not be reading it — so No there
cannot commit anything. The workspace-removal prompt is guarded when the row has
a live terminal *and* the `event` axis resolves to No (the shipped default), and
answering a guarded one defers (the silence `Esc`
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

Read the guard off the resolved `ConfirmStyle`, not off "is a terminal live":
under `event: yes` the user has asked for a Yes default here, which makes their
No a deliberate answer that may pin the keep. "This prompt's No must not
decide" and "this prompt's default is No" are the same condition, so deriving
one from the other is what keeps them from drifting apart.

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
