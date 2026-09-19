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

## Confirm modals do not guard against stray keys

`default_no()` is an alias that does nothing — Enter confirms. A Confirm is a
deliberate-intent prompt, not protection from a typo, so anything genuinely
destructive needs a stronger guard than a modal.

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
