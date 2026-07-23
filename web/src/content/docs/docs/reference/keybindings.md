---
title: Keybindings reference
description: The full keymap for every pane, leader chord, and picker.
---

<!-- GENERATED FILE — do not edit by hand.
     Regenerate with: LAZYBOX_REGEN_KEYMAP_DOCS=1 cargo test -p lazybox-tui --test keymap_docs
     Source: crates/tui/tests/keymap_docs.rs (renders the runtime action catalog). -->

The full default keymap, generated from the action catalog (`crates/tui-core/src/action.rs`). Press `?` in the app to open **Ask Lazybox**: typing searches your live bindings immediately, and Enter asks in plain language. Press `?` again at its empty prompt for the compact shortcut index. If you've remapped keys via `ui.action_keys` or selected a preset, those in-app surfaces show your effective bindings; this page shows the defaults.

Grouped actions live behind **leader keys** (press the leader, a which-key menu pops up, the next key picks the action). The default keymap is leaders-only: no single-key aliases for grouped actions.

## The design

The keymap follows five rules:

1. **Ask, don't memorize.** `?` searches the effective keymap and answers workflow questions; the static index is secondary.
2. **Common verbs stay direct.** `Enter` opens, `s` starts a shell, `e` opens the editor, `r` replies, `z` snoozes, and `/` searches.
3. **Families use mnemonic leaders.** `w` work, `a` agent, `b` main branch, `g` GitHub, and `x` workspace management. Every leader opens a visible menu.
4. **Risk requires intent.** Destructive/provider mutations are behind a leader and confirmation; quit is `q q`.
5. **Terminal mode has one escape namespace.** `]]` opens every lazybox command that must work while the embedded program owns the keyboard.

## Global

Work from any non-terminal pane. A focused terminal forwards keys to the PTY; press `]]q` to return first. **Text selection (`F8`) is the deliberate exception** and works inside a terminal too.

| Key | Action | What it does |
| --- | --- | --- |
| `Tab` | cycle panes | Move focus to the next pane. |
| `Shift-R` | refresh | Re-poll every provider for fresh tasks. |
| `Ctrl-l` | redraw | Clear the terminal and repaint the whole UI from scratch. |
| `,` | settings | Open the Settings palette. |
| `t` | theme | Open the theme picker — arrow through the built-in palettes with a live preview, Enter to keep one. |
| `]` | snippets | Browse the snippet library — every `]]s<key>` shortcut with its description and body, so you can see what's available without already knowing the key. |
| `?` | ask lazybox | Search the live keymap or ask how to use lazybox in plain language. |
| `Shift-T` | tour | Launch the guided onboarding walkthrough (start from scratch, inbox, putting an agent on a task, juggling sessions, config). |
| `Shift-D` | sync diagnostics | Show recent provider-sync outcomes, last poll times, and errors. |
| `Shift-M` | messages | Open the messages log — a scrollable, clearable history of recent footer notices, so an error that flashed and faded is still readable. |
| `Esc` | dismiss | Clear the current footer notice, whatever its severity — retryable, info, permanent, or auth. |
| `` ` `` | jump to workspace | Open a fuzzy picker over every workspace (across repos) and jump to the one you pick. |
| `!` | next asking | Jump the cursor to the next workspace whose agent is waiting on input (a quick jump; the workspace picker `` ` `` reaches any workspace). |
| `Shift-F` | next failing | Jump the cursor to the next PR whose CI is failing (a quick jump; the workspace picker `` ` `` reaches any workspace). |
| `.` | focus mode | Maximize the focused workspace's terminal to near-fullscreen behind a slim event header, hiding the sidebar and activity pane. |
| `Shift-W` | start work | Pick a project, name a workspace, and start the default agent in it — all in one step, from any pane. |
| `Shift-P` | activity pane | Show or hide the activity pane. |
| `F8 \| Alt-s \| Ctrl-Alt-s` | text selection | Toggle lazybox's mouse capture so the host terminal regains native text selection (trackpad-select + Cmd-C in agent scrollback). |
| `Shift-Arrows` | resize splitters | Grow / shrink the focused splitter. |
| `q q` | quit *(two-press chord)* | Quit lazybox. |

## Workspace

Act on the focused workspace. Available from the sidebar **and** the activity pane (the sidebar selection stays the reference frame while reading activity).

| Key | Action | What it does |
| --- | --- | --- |
| `Enter` | open | Focus the workspace's activity / terminal. |
| `s` | shell | Open a shell in the workspace's worktree. |
| `e` | editor | Open the worktree in the configured editor. |
| `m` | mark read | Mark every activity row on the focused workspace read. |
| `z` | snooze | Snooze the workspace for ~4h (toggle). |
| `r` | reply | Open the reply textarea targeted at this workspace. |
| `n` | notes | Edit this workspace's local scratchpad — a private note that never syncs to a provider. |

## Sidebar

Manage the sidebar list itself — only while the sidebar has focus.

| Key | Action | What it does |
| --- | --- | --- |
| `f` | filter | Open the filter menu — toggle state (with-agent, CI-failing, conflict, unread, asking, …), role, and kind predicates. |
| `o` | order | Cycle the sort order (recency → by-role → by-role with section headers). |
| `Shift-S` | switch mailbox | Cycle the mailbox view (Inbox → Inactive → Snoozed). |
| `/` | search | Open the incremental search bar scoped to the focused project. |
| `Space` | collapse group | Collapse or expand the repo group the cursor is in — fold a project's workspaces into a single header row, and unfold it again. |
| `v` | select | Toggle the focused workspace in/out of the multi-select set. |
| `Shift-B` | broadcast | Send one instruction — a snippet, free text, or both — to every multi-selected workspace at once. |

`j` / `k` (or arrows) move the cursor; `Esc` clears a `v` multi-selection.

## Activity

Only while the activity (right) pane has focus.

| Key | Action | What it does |
| --- | --- | --- |
| `Enter` | toggle section | Collapse / expand the activity section. |
| `→/←` | expand/collapse | Expand or collapse the focused activity row. |
| `g` | top | Jump the activity cursor to the first row. |
| `Shift-G` | bottom | Jump the activity cursor to the last row. |
| `d` | description | Toggle the PR / issue description visibility. |
| `Space` | select row | Toggle the focused activity row in/out of the multi-select set (also `v`). |
| `z` | undo mark-read | Re-unread the most recent auto-marked row. |

`j` / `k` (or arrows) move the row cursor; `→`/`l` expand and `←`/`h` collapse the focused row; `w w` works on the selection.

## Terminal

A focused terminal forwards every key to the PTY; only the chords below are intercepted. `]]` (the escape char, doubled) opens the terminal command menu.

| Key | Action | What it does |
| --- | --- | --- |
| `Shift-PgUp/Dn` | scroll | Scroll the terminal's scrollback buffer. |
| `]]q` | exit to sidebar | `]]` is a non-timed leader from the terminal: `]]q` exits to the sidebar, `]]s` opens snippets, `]]f` toggles focus. |

### The `]]` terminal leader

`]]` is a non-timed leader: after the two presses it waits for the command key. `Esc` or any unbound key cancels back into the terminal; a lone `]` followed by any other key is sent to the program verbatim. The escape char is configurable (`terminal.escape_char`; the legacy `ui.terminal_escape_char` alias is still accepted).

| Chord | Action |
| --- | --- |
| `]]s` | Open the snippet picker (typing a full key auto-submits — `]]srev`) |
| `]]r` | Restore the in-flight draft, or the last submitted agent prompt, without sending it |
| `]]f` | Toggle focus mode |
| `]]q` | Exit to the sidebar |
| `` ]]` `` | Open the fuzzy jump-to-workspace picker |
| `]]1…9` | Jump to the Nth agent workspace (sidebar order) |
| `]]\|` | Split the focused tile side-by-side (`]]\` is an alias) |
| `]]-` | Split the focused tile stacked |
| `]]←↓↑→` | Move tile focus; Left/Right cycles tabs in Tabs mode |
| `]]x` | Close the focused terminal (tile or active tab) |
| `]]t` | Toggle whether the next terminal opens as a split or a tab; persists `ui.terminal_new_layout` |

### Scrollback

| Key | Action |
| --- | --- |
| `Shift-PgUp` / `Shift-PgDn` | Scroll the scrollback |
| `Shift-Home` / `Shift-End` | Jump to the top / bottom |
| `Ctrl-c` | Forwarded to the program as an interrupt |

## Leader menus

Press the leader key, then the second key. Every menu shows a which-key popup while it waits.

### `w` — work

`w` opens a deterministic work menu: press `w w` for the default or already-running agent, or choose an agent / model tier below. Nothing waits on a timeout, so the second key acts immediately.

| Chord | Action |
| --- | --- |
| `w w` | work on this |
| `w c` | work in claude |
| `w x` | work in codex |
| `w u` | work in cursor |
| `w S` | Haiku |
| `w M` | Sonnet |
| `w L` | Opus |

### `a` — agent

| Chord | Action |
| --- | --- |
| `a c` | spawn claude |
| `a x` | spawn codex |
| `a u` | spawn cursor |
| `a S` | Haiku |
| `a M` | Sonnet |
| `a L` | Opus |

### `b` — main branch

| Chord | Action |
| --- | --- |
| `b s` | shell on main *(confirmed first)* |
| `b c` | claude on main *(confirmed first)* |
| `b x` | codex on main *(confirmed first)* |
| `b u` | cursor on main *(confirmed first)* |

### `g` — github

| Chord | Action |
| --- | --- |
| `g m` | merge PR *(confirmed first)* |
| `g g` | auto-merge on green |
| `g p` | policies |
| `g r` | reviewers |
| `g a` | assignees |
| `g l` | labels |
| `g o` | open in browser |
| `g d` | delete / close *(confirmed first)* |

### `x` — workspace

| Chord | Action |
| --- | --- |
| `x n` | new workspace |
| `x p` | new project |
| `x i` | import checkout |
| `x a` | adopt sessions |
| `x j` | join into PR |
| `x z` | long snooze *(confirmed first)* |
| `x x` | archive *(confirmed first)* |
| `x c` | close issue *(confirmed first)* |

## Mouse

- Click any pane to focus it; drag a splitter to resize.
- Right-click a sidebar row for the context menu; right-click a URL / path / `#N` reference inside a terminal to open it.
- The wheel scrolls the pane under the cursor (terminal scrollback included).
- The mouse-capture toggle (`F8` / `Alt-s` / `Ctrl-Alt-s`) hands the mouse back to the host terminal for native text selection.

## Pickers

Every picker modal (snippets, jump-to-workspace, reviewers, labels, …) shares the same keys:

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Move the selection |
| `Enter` | Confirm |
| (type) | Filter the list |
| `Esc` | Dismiss |
