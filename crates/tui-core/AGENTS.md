# The action catalog

Ratatui-free TUI logic: the action catalog, intent resolvers, latches,
editors, platform shims, and the marker registry. Everything here is testable
without a terminal — keep it that way.

Read [`AGENTS.md`](../../AGENTS.md) first; pane and model structure is next
door in [`../tui/AGENTS.md`](../tui/AGENTS.md).

## One row per user action

`src/action.rs` holds one `ActionDef` per action, and keyboard dispatch,
footer hints, the context menu, the which-key popup and the `?` help all read
it. A row carries:

- **`chords: Vec<Chord>`** where `Chord = Key | Seq`. Every leader and
  double-press (`g m`, `q q`, `] ]`) is a `Seq`; multiple chords are
  alternatives, which is how a user override coexists with a default.
- **A `param`** (an agent id) that generates one real row per enabled agent at
  startup, rather than a special case in dispatch.
- **A `guard`** (`None | DoublePress | Confirm(prompt)`) carrying the `q q`
  double-tap and the confirm modals.

The `Model` builds a runtime catalog (static rows + generated agent rows +
overrides) and leader arming is a pure function of it (`seq_continuations`,
`find_action_for_seq` in `crates/tui/src/realm/model/helpers.rs`).

## Section is resolution scope

Each row's `Section` (`Global` / `Workspace` / `Sidebar` / `Activity` /
`Terminal`) doubles as its scope: `section_rank` maps `(Section, focus)` to a
priority. Cross-section shadowing is deliberate and focus-ranked; a collision
*within* a section is a genuine ambiguity, and a detector test fails the build
on one rather than leaving it as tribal knowledge.

The practical consequence: a per-row action belongs in `Section::Workspace`,
not `Section::Sidebar` — Workspace resolves from the activity pane too, so a
key placed in the wrong section simply does nothing where the user expects it.
When a key "does nothing", check resolution and availability before dispatch;
the `mastery:<action>` counters in the state DB tell a resolution failure from
a focus or state one.

## Policies that are decisions, not style

- **The default keymap is leaders-only.** A concept with two or more sibling
  actions gets a leader group (named by `leader_group_label`); only true
  primary actions earn a top-level key. Adding a direct alias for a grouped
  action re-opens a settled decision.
- **Pane-native keys are an allowlist.** Cursor navigation plus the small set
  in `PANE_NATIVE_KINDS` (`crates/tui/src/realm/model/keys.rs`) stay as pane
  match arms; everything else goes through the catalog so remaps and help stay
  honest.
- **Selection is the primary path, not a mode.** A bulk-appropriate workspace
  action reads `resolve_targets` (selection-or-focused), and a run that
  actually did something consumes the selection; a run where every target was
  ineligible keeps it so it can be retried. Inherently single-target actions
  stay focused-only.
- **`ui.keymap_preset`** selects an in-tree starter keymap; explicit
  `ui.action_keys` layers on top. Both must survive a new row without manual
  migration.

## The keymap reference is generated

`web/src/content/docs/docs/reference/keybindings.md` is rendered from this
catalog by `crates/tui/tests/keymap_docs.rs`, and the test fails on drift.
Never hand-edit the page, and never restate the per-key list in prose that
will rot beside it:

```bash
LAZYBOX_REGEN_KEYMAP_DOCS=1 cargo test -p lazybox-tui --test keymap_docs
```

The in-app `?` screen is generated from the same catalog, including the
mastery marks that turn it into a map of chords still worth learning. The
glyph legend is generated too, from `src/markers.rs` — the registry the
sidebar paints from, so the legend cannot drift from the rows.
