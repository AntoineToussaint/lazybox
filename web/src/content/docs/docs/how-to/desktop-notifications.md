---
title: Get desktop notifications
description: Control the OS notifications lazybox fires when an agent needs input or finishes, and pick how the banner is delivered — including over SSH.
---

When you're driving several agents, you don't want to babysit the TUI. lazybox
fires an **OS desktop notification** when an agent needs input, when an agent
finishes, and when a workspace raises a fresh attention signal (failing CI, a
requested review, new unread activity) — so you can look away and get pulled
back only when it matters.

## It's on by default

Desktop notifications are **enabled by default** (`attention.desktop_notify`
defaults to `true`). This is independent of the in-app `agent_asking` attention
badge — set `desktop_notify: false` to keep the badge but silence the OS banner:

```yaml
attention:
  desktop_notify: false   # keep the in-app badge, no OS banner
```

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
| `auto` (default) | Picks per environment: a dedicated helper (`terminal-notifier` / `notify-send`) when running locally, the terminal's OSC escape sequence over SSH — or when the only local helper is `osascript` and your terminal supports the escape sequence. |
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
