# TUI & UX

How lazybox is laid out and driven from the keyboard and mouse. This page covers
the pane structure, the action/keybinding system, the overlays (help, settings,
tour), the activity feed, reply, mouse handling, modals, and desktop
notifications.

See [`CLAUDE.md` § TUI tiers](../../CLAUDE.md) and
[`DESIGN.md` § Component tree](../../DESIGN.md) for the architecture. The full
key reference also lives in [`README.md`](../../README.md#key-bindings). Ask
Lazybox (`?`) is the always-current source: type to search the live keymap, or
press Enter to ask a workflow question.

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
**Config / flags:** `ui.action_keys` (per-action key overrides, incl. `spawn_agent.<id>` for agent chords), `ui.keymap_preset` (`default` / `vim`), `ui.quit_double_tap_window` (800ms), `terminal.escape_char`
**Key bindings:** `q q` quit; `w` work, `a` agent, `b` main branch, `g` GitHub, `x` workspace leaders; `]]` terminal leader

### What it does
A central **Action catalog** maps every command to keys, with support for
two-press chords (`q q`), five named leader groups, and user overrides.

### How to use it
Most common verbs are single presses (see each feature page). Chords: `q q` to
quit (double-tap within 800ms), or press a named leader and choose from its
visible menu—`g` then `m`/`g`/`r`/`a`/`l`/`o` for GitHub actions, for example.
Override any binding via `ui.action_keys` in config; agent spawn chords are
remapped there too, keyed `spawn_agent.<id>` (e.g. `spawn_agent.claude: "c"`
restores a top-level key).

### How it works (brief)
`Action` + `ActionDef::for_kind` (`crates/tui-core/src/action.rs`) is the
catalog; leader groups are multi-stroke `Chord::Seq` rows in it (the which-key
popup is a pure function of the armed prefix). Key routing is
`handle_pane_key` (`crates/tui/src/realm/model/keys.rs`); dispatch is
`dispatch_action_unchecked` (`dispatch.rs`). Effective keys honor `ui.action_keys`
overrides. `f` (filter menu), `o` (sort), and `/` (search) are catalog actions
and remappable; only per-pane cursor navigation (`j/k`, arrows) plus a small
allowlisted set of pane-native arms (`PANE_NATIVE_KINDS`,
`crates/tui/src/realm/model/keys.rs`) are handled inline — and some of those
don't honor remaps.

### Test checklist
- [ ] `q q` within 800ms quits; a single `q` does not.
- [ ] `g` opens the GitHub which-key group; `g m` merges.
- [ ] `Shift-M` no longer merges — `g m` is the only default merge chord.
- [ ] A `ui.action_keys` override remaps the bound action.
- [ ] Ask Lazybox search and the shortcut index reflect effective keys.

### Known sharp edges
- Catalog actions and inline pane handlers coexist; not every key is in the `Action` enum.
- Less-frequent workspace actions live behind the `x` which-key menu; use Ask Lazybox (`?`) for the complete live keymap.

---

## Ask Lazybox & shortcut index

**Status:** stable
**Crate(s):** `tui` (`realm/components/help_ask.rs`, `help.rs`)
**Config / flags:** reflects `ui.action_keys` overrides
**Key bindings:** `?`

### What it does
The primary help surface combines instant fuzzy search over the effective
keymap with conversational workflow answers. A secondary compact index groups
direct bindings by scope and advertises the five leader menus without repeating
every continuation.

### How to use it
Press `?` to open Ask Lazybox. Type to search immediately or press Enter to ask.
At an empty prompt, press `?` again to switch to the shortcut index; `?` there
returns to Ask. `Esc` closes either surface. Beyond explaining, Ask can *do* a
small, allowlisted set of things: ask it to "add a snippet …" or "switch to the
vim keymap" and it proposes the change as a confirm-with-preview, then — on
accept — applies it (writing a snippet and reloading it live, or persisting a
config key). Decline and nothing changes.

### How it works (brief)
Both surfaces build from the runtime catalog and render effective keys
(honoring overrides). Conversational answers receive that same catalog plus the
embedded feature docs as context. Ask uses the configured default agent when it
is Claude or Codex; otherwise the fallback order is Claude, then Codex. The
fuzzy search layer remains fully local and works without either CLI. Actions are
a fixed allowlist emitted as a `lazybox-action` block the agent proposes;
lazybox parses it, validates it against the allowlist (and against live state —
a theme must exist, an agent must be enabled), confirms with the user, and owns
every mutation — the agent never touches the filesystem. The set is
`add_snippet` (written + hot-reloaded) and `edit_config` (an allowlisted config
key: `ui.theme` and `setup.default_agent` apply live, `ui.keymap_preset` after a
restart).

### Test checklist
- [ ] `?` opens Ask Lazybox directly; another `?` toggles the compact index.
- [ ] Overridden keys show their effective binding.
- [ ] A Codex-only agent configuration can answer conversational help.
- [ ] Asking to add a snippet pops a confirm-with-preview; accept writes + hot-reloads it, decline changes nothing.
- [ ] Asking to change an allowlisted config key (`ui.theme`) pops a confirm; accept persists + live-applies it. An off-allowlist key or bad value is refused before any confirm.
- [ ] `Esc` dismisses either surface; the index also closes on non-navigation keys.

### Known sharp edges
- A focused embedded terminal owns ordinary keys; use `]]q` to return before opening Ask.

---

## Settings palette & setup wizard

**Status:** stable
**Crate(s):** `tui` (`setup_flow.rs`, `realm/setup_ctx.rs`), `config`
**Config / flags:** writes `~/.lazybox/config.yaml`
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
A guided walkthrough of lazybox's main workflows, shown once on first run and
re-openable on demand. Snippets get two dedicated cards: send a built-in
workflow with the `]]s<key>` fast path, then repeat it through persisted Recent
memory and read the per-workspace `]N` progress cue.

### How to use it
Press `Shift-T` to launch it; it also auto-runs the first time (`ui.tour_seen`
gates the auto-launch).

### Test checklist
- [ ] `Shift-T` opens the tour.
- [ ] The tour auto-runs on a fresh profile and sets `ui.tour_seen`.
- [ ] It doesn't auto-run again after being seen.
- [ ] Separate snippet cards cover fast send/preview and Recent/`]N` memory.
- [ ] The memory card demonstrates Ask Lazybox hot reload and `Shift-B`.

### Known sharp edges
- Newer surface; content may lag behind feature changes.

---

## Activity feed

**Status:** stable
**Crate(s):** `tui` (`components/activity_feed.rs`, `right_pane/`)
**Config / flags:** `ui.task_body_max_rows` (description clamp)
**Key bindings:** `j/k` (or `↑/↓`) navigate, `g/G` top/bottom, `h/l` (or `←/→`) collapse/expand row, `d` toggle description, `Enter` collapse/expand whole Activity section, `Space`/`v` multi-select, `m` mark read, `z` undo auto-mark, `PageUp/PageDown` screenful

### What it does
The right pane: the focused workspace's merged feed of comments, reviews, status
changes, and CI updates, with a collapsible Description section and per-card
expand/collapse. Multi-select drives bulk mark-read and the `w w`/reply targeting.

### How to use it
Navigate with `j/k`; `g/G` jump top/bottom; `h/l` collapse/expand the focused
card; `d` toggles the PR/issue description teaser — a second `d` on a long or
richly-formatted preview (tables, fenced code, images), or clicking
`+N more lines`, opens the full body in a scrollable markdown reader modal
(#448: headings/lists/code/links/tables, `j/k`·PgUp/PgDn·wheel scroll, click a
link to open it, `Esc` closes); `Enter` collapses/expands the whole
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
- [ ] `v` multi-selects rows; the footer shows the count; `w w`/reply target the set.
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

## Mouse handling

**Status:** stable
**Crate(s):** `tui` (`realm/model/keys.rs`, layout math)
**Config / flags:** `ui.split_step_percent`
**Key bindings:** `F8` / `Alt-s` / `Ctrl-Alt-s` toggle mouse capture; `Shift-arrows` resize

### What it does
lazybox captures the mouse for pane-scoped selection, clickable UI, splitter
drags, and wheel scrollback. Toggle capture off to hand the mouse to the host
terminal for native whole-screen selection.

### How to use it
- Click to focus panes / select rows; double-click activity cards to expand;
  right-click a sidebar row for a context menu; right-click terminal content to
  open a detected URL/file/issue reference.
- Drag a splitter to resize; mouse wheel scrolls the focused list/terminal.
- `F8` (or `Alt-s` / `Ctrl-Alt-s`) toggles lazybox's mouse capture; off = host-native selection, on = lazybox pane-scoped selection + splitter drag.

### How it works (brief)
Mouse events route through `crates/tui/src/realm/model/keys.rs`: hit-testing for
panes/splitters (±1 cell tolerance), drag-select with OSC 52 copy on release,
wheel scroll with inertia damping, and the capture toggle. The context menu is a
`SidebarContext` modal.

### Test checklist
- [ ] Clicking selects/focuses the expected pane or row.
- [ ] Dragging a splitter resizes; the layout persists for the session.
- [ ] Mouse wheel scrolls the focused list/terminal.
- [ ] `F8` flips capture; host-native selection works when off, lazybox selection when on.
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
**Crate(s):** `tui-core` (`src/platform.rs`, `src/notify.rs`), `tui` (`components/sidebar/handlers.rs`)
**Config / flags:** `attention.desktop_notify` (master), `attention.notifier` (`auto` | `osc` | `subprocess` delivery), `attention.{unread,ci_failing,review_pending}` (which events notify)
**Key bindings:** —

### What it does
Fires an OS notification when something needs attention while lazybox is
unfocused: an agent transitions to needing input, CI starts failing, a review
gets requested, or a new comment lands. You don't have to babysit a session or
keep checking GitHub.

### How to use it
On by default. Agent prompts read `lazybox — <workspace> needs input`; provider
events read `lazybox — CI failing on <workspace>` / `… review requested on …` /
`… new activity on …`. A footer notice also appears in-app for agent prompts.
Disable all OS banners with `attention.desktop_notify: false` (the footer notice
stays). Which provider events notify follows the per-signal `attention` flags
(`ci_failing`, `review_pending`, `unread`) that already gate the in-app badge.

### How it works (brief)
Notifications funnel through `platform::notify_user`
(`crates/tui-core/src/platform.rs`). Triggers: the sidebar detects the
Active→Asking edge from `Event::AgentState` and the rising edge of attention
signals on `Event::WorkspaceUpserted`
(`crates/tui/src/components/sidebar/handlers.rs`).

Delivery is picked by `attention.notifier` (default `auto`): in a **local
session** a dedicated helper runs when present — `terminal-notifier` (grouped)
on macOS, `notify-send` on Linux — it's verifiable (helper exit status lands
in the log) and immune to terminal OSC quirks. Without one, a recognized
OSC-capable terminal's own banner is preferred over the `osascript` fallback,
because `display notification` exits 0 even when macOS suppresses the banner
(Script Editor permission denied, Focus mode) — osascript is the last resort
for unrecognized terminals on a stock Mac; Windows is a stub. **Over SSH**
(where a helper would banner the remote host) the
**terminal's own OSC notification sequence** is used instead
(`crates/tui-core/src/notify.rs`): Ghostty / Kitty / WezTerm get OSC 777
(`ESC]777;notify;TITLE;BODY`), iTerm2 gets OSC 9 (body only), detected via
`$TERM_PROGRAM`; the local emulator renders the banner. Inside tmux the
sequence is wrapped in a passthrough envelope (requires `allow-passthrough`,
default-on in tmux 3.3a). `notifier: osc` / `notifier: subprocess` force one
path. OSC sequences are never written at the point of the triggering event —
they're queued and emitted between frames on the render thread, so the escape
bytes can't interleave with a ratatui frame flush and paint as literal junk
(#296). Every attempt logs its chosen backend at debug level in
`/tmp/lazybox.log`.

Banners are suppressed while lazybox's terminal is reported focused (DEC mode
1004 focus reporting) so it doesn't self-spam — a terminal that never reports
focus is treated as unfocused so it still notifies.

### Test checklist
- [ ] An agent moving Working → InputNeeded fires an OS notification while unfocused.
- [ ] CI flipping green → failing on a tracked workspace fires one banner; staying failing does not re-notify.
- [ ] A workspace seen for the first time (already failing) does not fire a startup banner.
- [ ] The in-app footer notice appears regardless of OS banner support.
- [ ] `attention.desktop_notify: false` suppresses every OS banner but keeps the footer.
- [ ] On Ghostty/iTerm2, the banner appears with no `terminal-notifier` installed.

### Known sharp edges
- macOS subprocess fallback has no bundle id yet (no custom icon); `terminal-notifier` must be installed for grouped banners, else it falls back to `osascript`.
- tmux passthrough needs `allow-passthrough on` (default since tmux 3.3a) for OSC banners to reach the outer terminal.
- A provider-event signal that's already present when a workspace first appears seeds the baseline silently — only the rising edge notifies, so a second unread comment before you've read the first won't re-notify.
- Windows notifications are not implemented.
