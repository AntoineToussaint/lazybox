---
title: Keybindings reference
description: The full keymap for every pane, leader chord, and picker.
---

The full keymap. Each pane declares its own keymap; the bottom hint bar reads
from the focused pane. Global keys work everywhere **except** a focused
terminal, which forwards every key to the PTY — press `]]` to return to the
sidebar first.

These bindings come from the action catalog (`crates/tui-core/src/action.rs`),
which is also what the in-app `?` help renders. If you've remapped keys via
`ui.action_keys` in your config, `?` shows your live bindings; this page shows
the defaults.

## Global

| Key | Action |
| --- | --- |
| `Tab` | Cycle panes |
| `?` | Help |
| `,` | Settings palette |
| `q q` | Quit (two-press chord) |
| `Shift-R` | Manual refresh — re-poll every provider |
| `Shift-T` | Launch the guided tour |
| `Shift-D` | Sync status (last poll times, errors) |
| `Shift-W` | Start an agent from anywhere (pick project → name → spawn) |
| `!` | Jump to the next workspace whose agent is waiting |
| `Shift-F` | Jump to the next PR with failing CI |
| `Shift-arrows` | Resize the splitters |
| `F8` / `Alt-s` / `Ctrl-Alt-s` | Toggle mouse capture (host-native selection) |

Mouse: click any pane to focus it, drag a splitter to resize.

## Sidebar (workspace list)

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Navigate |
| `Enter` | Open the workspace (focus activity) |
| `Space` | Fold / unfold the repo group |
| `w` | "Work" — spawn Claude with the right prompt for the row's state (fix CI / fix conflict / address comments / implement issue) |
| `c` | Spawn Claude Code |
| `x` | Spawn Codex |
| `u` | Spawn Cursor |
| `s` | Spawn a shell |
| `e` | Open the worktree in your editor |
| `m` | Mark all of this workspace read |
| `z` | Snooze (~4h, toggle) |
| `Shift-Z` | Long snooze |
| `f` | Cycle the role filter (all → author → reviewer → assignee → mentioned) |
| `o` | Cycle the sort order (recent → by-role → split) |
| `Shift-S` | Cycle mailbox (Inbox → Inactive → Snoozed) |
| `/` | Search |
| `n` | New pre-PR workspace |
| `Shift-N` | New project (pick a tracked repo, or create a local project) |
| `Shift-A` | Adopt sessions — move sessions into another workspace |
| `Shift-J` | Join an issue into the PR that closes it |
| `Shift-X` | Archive (kills sessions — destructive) |

### GitHub (`g` leader)

`g` is a leader key: press it to open the **github** which-key popup, then the
second key. The `Shift-*` forms are direct aliases for the same actions.

| Chord | Alias | Action |
| --- | --- | --- |
| `g m` | — | Merge the PR (when CI green, approved, no conflicts) |
| `g v` | `Shift-V` | Request reviewers |
| `g a` | `Shift-G` | Change assignees |
| `g l` | `Shift-L` | Manage labels |
| `g o` | `Shift-O` | Open the PR / issue in your browser |

## Activity pane

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Move the row cursor |
| `g` / `G` | Top / bottom |
| `→` / `l` | Expand the row |
| `←` / `h` | Collapse the row |
| `Enter` | Toggle the section |
| `Space` / `v` | Multi-select rows |
| `w` | Work on the selection |
| `d` | Toggle the PR / issue description |
| `m` | Mark the focused row read |
| `z` | Undo the auto-mark-read |
| `r` | Reply |

## Terminals

The terminal forwards every key to the PTY. Only the keys below are intercepted.

| Key | Action |
| --- | --- |
| (any key) | Forwarded to the PTY |
| `Ctrl-c` | Sent as an interrupt |
| `]]` (two presses) | Return to the sidebar |
| `]]<key>` | Open the snippet picker (fuzzy-filtered by key) |
| `]` then non-`]` | Sent to the agent verbatim |
| `]]\|` / `]]-` | Split the tile (vertical / horizontal) |
| `]]<arrow>` | Move tile focus (cycle tabs in Tabs mode) |
| `]]x` | Close the focused terminal (tile or active tab) |
| `Shift-PgUp` / `Shift-PgDn` | Scroll the scrollback |
| `Shift-Home` / `Shift-End` | Jump scrollback top / bottom |

The mouse wheel scrolls the scrollback too.

## Pickers

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Move the selection |
| `Enter` | Confirm the selection |
| (type) | Filter the list |
| `Esc` | Dismiss the picker |
