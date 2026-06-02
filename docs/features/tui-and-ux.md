# TUI & UX

How pilot is laid out and driven from the keyboard and mouse. This page covers
the pane structure, the action/keybinding system, the overlays (help, settings,
tour), the activity feed, reply, detach, mouse handling, modals, and desktop
notifications.

See [`CLAUDE.md` § TUI tiers](../../CLAUDE.md) and
[`DESIGN.md` § Component tree](../../DESIGN.md) for the architecture. The full
key reference also lives in [`README.md`](../../README.md#key-bindings); the
help overlay (`?`) is the always-current source.

---

## Three-pane layout & focus

**Status:** stable
**Crate(s):** `tui` (`src/pane.rs`, `realm/model/`)
**Config / flags:** `ui.split_step_percent` (resize step)
**Key bindings:** `Tab` cycle, `Shift-arrows` resize splitters

### What it does
Three regions: **Sidebar** (workspace list), **Activity** (right pane, the
focused workspace's feed), and **Terminals** (the embedded terminal stack).
`Tab` cycles focus through them.

### How to use it
`Tab` moves Sidebar → Activity → Terminals → Sidebar. Click any pane to focus
it. `Shift-arrows` resize the splitters; drag a splitter with the mouse for
continuous resize.

### How it works (brief)
The `Pane` trait (`crates/tui/src/pane.rs`) is a focusable region owning a
keymap, border, and key handling. The model holds Sidebar/Right/Terminals as
concrete fields and tracks focus as one `PaneFocus`
(`crates/tui/src/realm/model/mod.rs`); `next()` cycles it. The bottom hint bar
reads the focused pane's keymap.

### Test checklist
- [ ] `Tab` cycles Sidebar → Activity → Terminals → Sidebar.
- [ ] Clicking a pane focuses it.
- [ ] `Shift-Left/Right` resize the sidebar–right splitter; `Shift-Up/Down` the activity–terminal splitter.
- [ ] Dragging a splitter resizes continuously.
- [ ] The hint bar shows the focused pane's bindings.

### Known sharp edges
- `Tab` is context-dependent inside a terminal (see [terminal interaction](terminals-and-agents.md#terminal-interaction-model)).

---

## Key-binding system & chords

**Status:** stable
**Crate(s):** `tui-core` (`src/action.rs`), `tui` (`realm/model/keys.rs`, `dispatch.rs`)
**Config / flags:** `ui.action_keys` (per-action key overrides), `agent_shortcuts`, `ui.quit_double_tap_window` (800ms), `ui.terminal_escape_char`
**Key bindings:** `q q` quit, `g …` GitHub leader group, `]<key>` snippet trigger

### What it does
A central **Action catalog** maps every command to keys, with support for
two-press chords (`q q`), leader-key groups (`g` opens a which-key GitHub group),
and user overrides.

### How to use it
Most keys are single presses (see each feature page). Chords: `q q` to quit
(double-tap within 800ms), `g` then `m`/`v`/`a`/`l`/`o` for GitHub actions.
Override any binding via `ui.action_keys` in config; add single-char agent keys
via `agent_shortcuts`.

### How it works (brief)
`Action` + `ActionDef::for_kind` (`crates/tui-core/src/action.rs`) is the
catalog; `ActionGroup::Github` defines the leader group. Key routing is
`handle_pane_key` (`crates/tui/src/realm/model/keys.rs`); dispatch is
`dispatch_action_unchecked` (`dispatch.rs`). Effective keys honor `ui.action_keys`
overrides. Some sidebar/activity keys (`j/k`, `f`, `o`, `/`) are handled inline
in their pane handlers rather than as catalog actions.

### Test checklist
- [ ] `q q` within 800ms quits; a single `q` does not.
- [ ] `g` opens the GitHub which-key group; `g m` merges.
- [ ] `Shift-M` still works as a direct alias for `g m`.
- [ ] A `ui.action_keys` override remaps the bound action.
- [ ] The help overlay reflects effective (overridden) keys.

### Known sharp edges
- Catalog actions and inline pane handlers coexist; not every key is in the `Action` enum.
- Some `Shift-*` bindings (`Shift-T`, `Shift-N`, `Shift-A`, `Shift-J`, `Shift-F`, `Shift-Z`, `Shift-S`) aren't in the README tables — see help (`?`).

---

## Help overlay

**Status:** stable
**Crate(s):** `tui` (`realm/components/help.rs`)
**Config / flags:** reflects `ui.action_keys` overrides
**Key bindings:** `?`

### What it does
A which-key-style overlay listing the active bindings, grouped Global /
Workspace / Activity / Terminal — the always-current key reference.

### How to use it
Press `?`; dismiss with any key.

### How it works (brief)
`Help` (`crates/tui/src/realm/components/help.rs`) builds sections from
`ActionDef::all()` and renders effective keys (honoring overrides).

### Test checklist
- [ ] `?` opens the overlay with all four sections populated.
- [ ] Overridden keys show their effective binding.
- [ ] Any keystroke dismisses it.

### Known sharp edges
- Inline-handled keys (e.g. `/` search) may not all appear if they aren't catalog actions.

---

## Settings palette & setup wizard

**Status:** stable
**Crate(s):** `tui` (`setup_flow.rs`, `realm/setup_ctx.rs`), `config`
**Config / flags:** writes `~/.pilot/config.yaml`
**Key bindings:** `,`

### What it does
The setup wizard runs on first launch (detect tools → enable providers → enable
agents → set filters → pick scopes). Pressing `,` later opens the settings
palette to change any of it — add a repo, edit roles, pick agents, toggle skip-
permissions, clean worktrees — without nuking state.

### How to use it
First launch walks you through setup automatically. Press `,` any time for the
palette; choose an action (Edit scopes / Edit filters / Edit providers / Edit
agents / Toggle skip-permissions / Full setup / Clean worktrees / Inspect
worktrees).

### How it works (brief)
Flows are state machines over generic modals (`ChoiceModal`, `InputModal`,
`ConfirmModal`, `LoadingModal`). `SetupRunner` / `setup_ctx.rs`
(`crates/tui/src/realm/setup_ctx.rs`) drive steps; `SettingsAction` enumerates
palette entries. Output persists to `config.yaml` (`PersistedSetup`).

### Test checklist
- [ ] First launch (no config) runs the full wizard and writes `config.yaml`.
- [ ] `,` opens the palette with all actions.
- [ ] Edit scopes adds/removes a repo and it shows up in the inbox after a poll.
- [ ] Toggle skip-permissions flips `agent.skip_permissions` and persists.
- [ ] Clean/Inspect worktrees lists orphans and removes selected ones.

### Known sharp edges
- Editing config via the palette and hand-editing `config.yaml` can race — the palette writes the merged result.

---

## Guided tour

**Status:** beta
**Crate(s):** `tui` (`Tour` modal)
**Config / flags:** `ui.tour_seen`
**Key bindings:** `Shift-T`

### What it does
A guided walkthrough of pilot's main features (inbox, work, snippets,
navigation, config), shown once on first run and re-openable on demand.

### How to use it
Press `Shift-T` to launch it; it also auto-runs the first time (`ui.tour_seen`
gates the auto-launch).

### Test checklist
- [ ] `Shift-T` opens the tour.
- [ ] The tour auto-runs on a fresh profile and sets `ui.tour_seen`.
- [ ] It doesn't auto-run again after being seen.

### Known sharp edges
- Newer surface; content may lag behind feature changes.

---

## Activity feed

**Status:** stable
**Crate(s):** `tui` (`components/activity_feed.rs`, `right_pane/`)
**Config / flags:** `ui.task_body_max_rows` (description clamp)
**Key bindings:** `j/k` (or `↑/↓`) navigate, `g/G` top/bottom, `h/l` (or `←/→`) collapse/expand row, `d` toggle description, `Enter/Space/o` collapse/expand whole Activity section, `v` multi-select, `m` mark read, `z` undo auto-mark, `PageUp/PageDown` screenful

### What it does
The right pane: the focused workspace's merged feed of comments, reviews, status
changes, and CI updates, with a collapsible Description section and per-card
expand/collapse. Multi-select drives bulk mark-read and the `w`/reply targeting.

### How to use it
Navigate with `j/k`; `g/G` jump top/bottom; `h/l` collapse/expand the focused
card; `d` toggles the PR/issue description; `Enter` collapses/expands the whole
Activity section; `v` (or `Space`/click) toggles multi-select; `m` marks read;
`z` undoes the last auto-mark; double-click a card to expand/collapse it.

### How it works (brief)
`ActivityFeed` (`crates/tui/src/components/activity_feed.rs`) holds `cursor`,
`expanded`, and `selected` sets; index 0 is newest. The right pane renders it
plus the description/activity section headers (clickable). Activity comes from
the merged `Workspace.activity`.

### Test checklist
- [ ] `j/k` move the cursor and the view follows.
- [ ] `g/G` jump to top/bottom; `PageUp/PageDown` move a screenful.
- [ ] `h/l` collapse/expand the focused card; double-click does the same.
- [ ] `d` toggles the description section.
- [ ] `v` multi-selects rows; the footer shows the count; `w`/reply target the set.
- [ ] `m` marks the focused (or selected) rows read; `z` undoes the last auto-mark.

### Known sharp edges
- The description toggle key is `d` (formerly `b`); older muscle memory / docs may say `b`.

---

## Reply

**Status:** stable
**Crate(s):** `tui` (`realm/model/modals.rs`, `components/textarea.rs`), providers
**Config / flags:** —
**Key bindings:** `r` (from Sidebar or Activity)

### What it does
Opens a multi-line textarea targeted at the selected workspace (or the selected
activity rows) to post a reply through the provider's comment API.

### How to use it
Press `r`. Type your reply; `Ctrl-Enter` / `Ctrl-S` submit, `Enter` adds a
newline, `Esc`/`Ctrl-C` cancel. Readline bindings work (Ctrl-A/E, Alt-B/F,
Ctrl-K/U/W). From the Activity pane with rows multi-selected, the reply targets
that thread/selection.

### How it works (brief)
`mount_reply` (`crates/tui/src/realm/model/modals.rs`) mounts a `Textarea`
header'd with the target; submit dispatches `Command::PostReply { session_key,
body }`, which the provider turns into a GitHub/Linear comment (or Slack
message).

### Test checklist
- [ ] `r` from the sidebar opens the reply textarea targeted at the workspace.
- [ ] `r` from Activity with a row selected targets that comment thread.
- [ ] `Ctrl-Enter`/`Ctrl-S` submits and posts via the provider.
- [ ] `Esc` cancels without posting.
- [ ] The posted comment appears on the next poll.

### Known sharp edges
- Reply support depends on the provider implementing the comment mutation; unsupported providers no-op.

---

## Pane detach

**Status:** beta
**Crate(s):** `tui` (`pane.rs`, `realm/model/keys.rs`)
**Config / flags:** —
**Key bindings:** `Ctrl-Shift-D`

### What it does
Spawns a new pilot window pinned to the focused pane — e.g. pop a terminal out
into its own window.

### How to use it
Focus a pane and press `Ctrl-Shift-D`.

### How it works (brief)
`focused_detach_spec()` builds a `DetachSpec { layout, args }`
(`crates/tui/src/pane.rs`); `spawn_detached_pilot` launches a new pilot process
with those args (`crates/tui/src/realm/model/keys.rs`).

### Test checklist
- [ ] `Ctrl-Shift-D` on a terminal opens a new pilot window showing that pane.
- [ ] The detached window connects to the same daemon/state.
- [ ] Distinct from `Shift-D` (sync status).

### Known sharp edges
- Spawns a whole new process; both windows talk to the same daemon, so state stays consistent but you now manage two windows.

---

## Mouse handling

**Status:** stable
**Crate(s):** `tui` (`realm/model/keys.rs`, layout math)
**Config / flags:** `ui.split_step_percent`
**Key bindings:** `F8` / `Alt-s` / `Ctrl-Alt-s` toggle mouse capture; `Shift-arrows` resize

### What it does
pilot captures the mouse for pane-scoped selection, clickable UI, splitter
drags, and wheel scrollback. Toggle capture off to hand the mouse to the host
terminal for native whole-screen selection.

### How to use it
- Click to focus panes / select rows; double-click activity cards to expand;
  right-click a sidebar row for a context menu; right-click terminal content to
  open a detected URL/file/issue reference.
- Drag a splitter to resize; mouse wheel scrolls the focused list/terminal.
- `F8` (or `Alt-s` / `Ctrl-Alt-s`) toggles pilot's mouse capture; off = host-native selection, on = pilot pane-scoped selection + splitter drag.

### How it works (brief)
Mouse events route through `crates/tui/src/realm/model/keys.rs`: hit-testing for
panes/splitters (±1 cell tolerance), drag-select with OSC 52 copy on release,
wheel scroll with inertia damping, and the capture toggle. The context menu is a
`SidebarContext` modal.

### Test checklist
- [ ] Clicking selects/focuses the expected pane or row.
- [ ] Dragging a splitter resizes; the layout persists for the session.
- [ ] Mouse wheel scrolls the focused list/terminal.
- [ ] `F8` flips capture; host-native selection works when off, pilot selection when on.
- [ ] Right-click on a sidebar row opens the context menu.
- [ ] Drag-select in a terminal copies via OSC 52 (footer confirms).

### Known sharp edges
- Three capture-toggle bindings exist (`F8`, `Alt-s`, `Ctrl-Alt-s`) because terminals disagree on what reaches the app.

---

## Pickers / modals

**Status:** stable
**Crate(s):** `tui` (`realm/components/{choice,input,confirm,loading,textarea,error}.rs`)
**Config / flags:** —
**Key bindings:** picker keys: `j/k`/`↑↓` navigate, `Space` toggle (multi-select), `Enter` confirm, `PageUp/Down` & `Ctrl-u/d` jump, `Home/End`/`g/G` ends, `Backspace` previous step, `Esc`/`Ctrl-c` cancel

### What it does
A small set of reusable modal primitives — single/multi-select **Choice**,
single-line **Input**, **Confirm**, async **Loading** spinner, multi-line
**Textarea**, and **Error** — that all the wizards, pickers, and confirms are
built from, so navigation is consistent everywhere.

### How to use it
Any picker (scope/agent/repo/editor, settings palette, snooze duration, labels)
uses these keys. Multi-select pickers toggle with `Space` and confirm with
`Enter`.

### How it works (brief)
Generic components in `crates/tui/src/realm/components/`. Modals live on a
z-stack (`modal_stack`); only the top one receives input. `LoadingModal` polls
an async future/channel via `tick()`.

### Test checklist
- [ ] A multi-select picker toggles items with `Space` and confirms with `Enter`.
- [ ] `Esc`/`Ctrl-c` cancels any modal.
- [ ] `Backspace` returns to the previous wizard step where applicable.
- [ ] A LoadingModal shows a spinner and resolves when its async work completes.
- [ ] Only the top modal on the stack receives input.

### Known sharp edges
- Deeply nested flows (wizard → loading → error) rely on the stack unwinding correctly; watch for a stuck modal if a step errors.

---

## Desktop notifications

**Status:** stable
**Crate(s):** `tui-core` (`src/platform.rs`), `tui` (`components/sidebar/handlers.rs`)
**Config / flags:** `attention.desktop_notify`
**Key bindings:** —

### What it does
Fires an OS notification when an agent transitions from Working to needing input,
so you don't have to babysit a long-running session.

### How to use it
On by default. The banner says `pilot — <workspace> needs input`; a footer
notice also appears in-app. Disable OS banners with `attention.desktop_notify:
false` (the footer notice stays).

### How it works (brief)
The sidebar detects the Active→Asking edge from `Event::AgentState`
(`crates/tui/src/components/sidebar/handlers.rs`) and calls
`platform::notify_user` (`crates/tui-core/src/platform.rs`): macOS prefers
`terminal-notifier` (grouped) and falls back to `osascript`; Linux uses
`notify-send`; Windows is a stub.

### Test checklist
- [ ] An agent moving Working → InputNeeded fires an OS notification.
- [ ] The in-app footer notice appears regardless of OS banner support.
- [ ] `attention.desktop_notify: false` suppresses the OS banner but keeps the footer.
- [ ] No duplicate notification for an agent that's already waiting.

### Known sharp edges
- macOS notifications have no bundle id yet (no custom icon); `terminal-notifier` must be installed for grouped banners, else it falls back to `osascript`.
- Windows notifications are not implemented.
