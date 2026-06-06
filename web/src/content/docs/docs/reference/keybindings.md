---
title: Keybindings reference
description: The full keymap for every pane and picker.
---

The full keymap. Each pane declares its own keymap; the bottom hint bar reads
from the focused pane. Global keys work everywhere.

## Global

| Key | Action |
| --- | --- |
| `Tab` | Cycle panes |
| `?` | Help |
| `,` | Settings palette |
| `q q` | Quit |
| `!` | Jump to the next workspace whose agent is waiting |
| `Shift-arrows` | Resize the splitters |
| `F8` / `Alt-s` | Toggle mouse capture |

## Sidebar (workspace list)

| Key | Action |
| --- | --- |
| `j` / `k` | Navigate |
| `Enter` | Open the workspace |
| `Space` | Fold / unfold the repo group |
| `s` | Spawn a shell |
| `c` | Spawn Claude Code |
| `x` | Spawn Codex |
| `u` | Spawn Cursor |
| `w` | "Work" — spawn Claude with the right prompt for the row's state (fix CI / fix conflict / address comments / implement issue) |
| `f` | Cycle the role filter (all → author → reviewer → assignee → mentioned) |
| `o` | Cycle the sort order (recent → by-role → split) |
| `m` | Mark all of this workspace read |
| `n` | New pre-PR workspace |
| `e` | Open the worktree in your editor |
| `Shift-R` | Manual refresh |
| `Shift-M` | Merge the PR |
| `Shift-X` | Archive |

## Activity pane

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Scroll |
| `g` / `G` | Top / bottom |
| `h` / `l` | Collapse / expand a comment |
| `v` | Multi-select |
| `m` | Mark read |
| `z` | Undo auto-mark-read |
| `b` | Toggle the description |

## Terminals

| Key | Action |
| --- | --- |
| (any key) | Forwarded to the PTY |
| `Ctrl-c` | Sent as SIGINT |
| `]]` (two presses) | Return to the sidebar |

## Pickers

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Move the selection |
| `Enter` | Confirm the selection |
| (type) | Filter the list |
| `Esc` | Dismiss the picker |
