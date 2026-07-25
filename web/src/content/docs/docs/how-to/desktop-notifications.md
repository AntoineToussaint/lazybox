---
title: Get desktop notifications
description: Fire an OS notification when an agent needs your input, and pick how the banner is delivered — including over SSH.
---

When you're driving several agents, you don't want to babysit the TUI waiting
for one to ask a question. lazybox can fire an **OS desktop notification the
moment an agent needs input**, so you can look away and get pulled back only
when it matters.

## Turn it on

Set `attention.desktop_notify` in `~/.lazybox/config.yaml`:

```yaml
attention:
  desktop_notify: true
```

This is independent of the in-app `agent_asking` attention badge — you can have
the badge without the banner, or both.

## Choose how the banner is delivered

Environments differ: a local terminal has helper binaries; an SSH session
doesn't. `attention.notifier` picks the delivery path:

```yaml
attention:
  desktop_notify: true
  notifier: auto   # auto | osc | subprocess
```

| Value | Behavior |
| --- | --- |
| `auto` (default) | Picks per environment: subprocess helpers when running locally, the terminal's OSC escape sequence when over SSH. |
| `subprocess` | Force a helper binary: `terminal-notifier` or `osascript` on macOS, `notify-send` on Linux. |
| `osc` | Force the terminal's OSC notification escape sequence — useful when the notification should come from wherever your terminal emulator runs (e.g. across SSH). |

For a local machine, `auto` is almost always right. Reach for `osc` when you run
the daemon on a remote host and want the banner on your laptop (see
[Remote over SSH](/docs/how-to/remote-over-ssh/)); reach for `subprocess` if
your terminal doesn't act on notification escape sequences.

## Requirements

- **macOS**: `terminal-notifier` (if installed) or the built-in `osascript`.
- **Linux**: `notify-send` (from `libnotify` — e.g. `sudo apt install
  libnotify-bin`).
- **OSC path**: a terminal emulator that acts on notification escape sequences.

If the chosen path has no working delivery mechanism, the in-app attention badge
still fires — you just won't get an OS banner.

## See also

- [Configuration reference → `attention`](/docs/reference/configuration/) — the
  full field list.
- [Remote over SSH](/docs/how-to/remote-over-ssh/) — where the `osc` notifier
  earns its keep.
