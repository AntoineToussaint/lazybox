# Minimal mobile sessions

Start with **`lb -m`**. Plain `lb` still runs the installed desktop release.
Mobile has two views: your sessions and the selected terminal. It starts directly
in the session list, without the provider onboarding wizard or an action menu.

The startup portal and **Ctrl-T Sessions** share one renderer and these bindings:

- **Session letter**: open that exact terminal. Labels are `a b c d e f g h i l m o q s t u v w y z`.
  **j, k, n, p, r, x are reserved** and never label sessions. `q` and `s` remain selectors.
- **k / j** or **Up / Down**: highlight a session without switching the running terminal.
  Page Up/Down and the mouse wheel also move the highlight. **[ / ]** or
  Shift-Tab / Tab move through groups of 20 sessions when there is overflow.
- **`n`**: create a session. Choose **No repository** or a known repository,
  then an enabled coding agent or **Shell**. Shell always remains available.
  The configured default agent comes first; choosing another does not change it.
- **`r`**: rename the highlighted chat. Its agent and shell entries share the name.
  **Ctrl-X** clears the input; Enter saves, Escape discards the edit.
- **`x`**: delete only the highlighted terminal after confirmation. **y**
  confirms; **n**, **Enter**, or **Escape** cancels. Workspace files and sibling
  sessions are kept. The confirmation captures the exact terminal ID.
- **`p`**: set the highlighted session's priority. Press a displayed letter to
  move it into that position (`a` for first, `b` for second), shifting the other
  sessions. Or use **j/k**, then **Enter**. **Escape** cancels priority mode.
  Labels skip the same reserved keys as Sessions; **[ / ]** pages destinations.
  The running terminal stays focused. Priority is saved in the current Lazybox
  profile (`ui.mobile_session_order`) and restored on subsequent launches.
  Existing tabs keep their order while the startup roster loads; new terminals
  append after saved tabs. Renaming a chat does not change its position.
  Desktop layout is unaffected. The last mobile reorder saved wins.
- **Enter**: open the highlighted session (also from the startup portal).
- **Escape** or **Ctrl-T**: close Sessions and return to the current terminal.
  With no sessions, the portal stays ready for `n`.
- **Ctrl-Q**: detach from Sessions while keeping processes running.

The one-column left rail shows live terminal states. Ctrl-T expands it into a
compact overlay without resizing or reflowing the running terminal. Tapping the
rail, header, or footer opens Sessions when terminal mouse input is enabled.
The collapsed rail keeps the current terminal visible and marks overflow with
arrows. The expanded panel highlights the target of rename/delete; the current
terminal's name is bold. Both selection highlights use neutral theme backgrounds.
The panel's status line describes the highlighted terminal, runner, and repository.

The startup portal fills the screen; the Ctrl-T panel covers only the rows it
needs. Both use the same session ordering, selectors, actions, and confirmation
flow. Displayed labels are updated with the live roster; a key targets the last
painted roster, so an unseen removal cannot redirect it to another terminal.
Canceling creation or rename returns to the same panel position. Escape from the
runner picker returns to the area picker; creation happens only after choosing
the final agent/Shell option. A completed creation closes the panel so the new
terminal can take focus.

**Ctrl-G** opens global settings from the portal or running terminal. It reuses
all desktop settings flows: Providers → Add / remove repos, Agents → Edit agents
(including Claude), Appearance → Change theme, and Maintenance. Use **h/l** for
sections, **j/k** for rows, Enter to pick, and Escape to return. Agent setup shows
installation/authentication status; enable Claude there before choosing it in
`n` → area → agent. In settings, **?** opens Ask Lazybox with mobile/global bindings;
Escape returns to settings. Letters remain text in question and other text inputs.

Creation and settings sheets hide the Sessions list without losing its cursor.
**Swipe** to scroll running chat history. Mouse wheel reports scroll vertically
across the whole mobile viewport, including its rail, header and footer. In
Sessions, swiping moves the highlight. Termius must forward mouse reports for
touch scrolling; native whole-screen panning does not generate Lazybox scroll
events. **Ctrl-D** jumps all the way to the live bottom without sending input to
the running program. Mobile does not bind Ctrl-U or Page Up/Page Down to chat
scrolling; those keys reach the running agent or shell.

Ordinary letters, Escape and Tab reach the running program. Buffered typing in
live mobile terminals survives slow frames rather than being discarded after
half a second. Sessions/settings shortcuts and stopped-terminal actions retain
the stale-input guard. Create shells through `n` → area → Shell.

Current rail indicators use the desktop state **colors**, with these symbol
simplifications (default theme colors are shown; other themes supply their own):

| State | Mobile | Color | Desktop sidebar |
| --- | --- | --- | --- |
| Working | `●` | Accent/blue | Animated braille spinner |
| Idle, no turn run yet | `○` | Muted | No symbol |
| Turn finished | `✓` | Success/green | `✓` |
| Needs input | `!` | Warning/amber | `?` |
| Usage limit | `!` | Warning/amber | `⧗` |
| No credit | `!` | Warning/amber | `¢` |
| Stalled on an error | `!` | Warning/amber | `↯` |
| Waiting for reset | `☾` | Muted | `☾` |
| Process exited | `×` | Muted | `✗` |
| Running shell/log viewer | `·` | Accent/blue | Separate runner badge |

`✓` means the agent finished its **turn**, not that the entire task is complete.
It remains available for further input. `↑` / `↓` indicate offscreen sessions,
not a process state. The active background uses `theme.fill` (a neutral slate in
Lazybox Dark), rather than `theme.hover`, which shares that theme's error red.

A normal `lb -m` launch attaches to the current daemon, or starts a detached local
one when needed. Reopening mobile reconnects to the same running processes and
terminal output. Closing mobile only closes its connection. If mobile attaches
to a daemon owned by an already-running desktop client, that desktop process
continues to own the daemon's lifetime. `lb --connect` attaches the laptop to a
standalone daemon started by mobile. Explicit `lb -m --connect <socket>` and relay
connection flags still work. A configured SSH tunnel is also honored.

Repository choices come from Lazybox's known GitHub repositories and local
projects with a configured checkout root. No repository is required for the
scratch option. Agents, repositories, and themes can be configured through the
shared settings sheets. Activity, restart, filters, and the earlier mobile action
menu are not part of the Sessions portal. Inside a running terminal, letters
remain normal input; open Sessions with Ctrl-T to use the management actions.

Session persistence here means closing/reopening the client or reconnecting SSH.
It does not keep a process running through a device reboot. Two attached clients
still share a terminal's PTY size. `lb -m --test` uses the existing disposable test
mode, which intentionally doesn't provide persistent sessions.

To build and install:

```sh
. "$HOME/.cargo/env"
CARGO_BUILD_JOBS=1 make build
./scripts/install-mobile.sh
```

The installer adds a small `lb` dispatcher and a separate mobile binary, retains
the official `lazybox` executable for plain `lb`, and removes the previous managed
`lb-m` / `lazybox-m` aliases. The original `lb` launcher is saved under
`~/.local/lib/lazybox-mobile/lb.desktop-original` on the first such installation.

The installer honors `CARGO_TARGET_DIR`. Set `LAZYBOX_BUILD_DIR` to use a specific
directory containing the built `lazybox` and `lb` executables.
