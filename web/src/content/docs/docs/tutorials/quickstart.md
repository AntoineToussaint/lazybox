---
title: Quickstart
description: Install lazybox and open your first workspace with a live terminal in about five minutes.
---

In a few minutes you will install lazybox, launch it, and open a workspace with
a live embedded terminal. That is the whole goal of this tutorial — one visible
win. Everything else can wait.

One prerequisite either way: the **GitHub CLI**, logged in. Run `gh auth login`
once — lazybox reads your token from `gh auth token`.

## 1. Install

The fastest path is a prebuilt binary.

**Homebrew** (macOS · Linux):

```sh
brew install AntoineToussaint/lazybox/lazybox
```

**Or `curl | sh`:**

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/AntoineToussaint/lazybox/releases/latest/download/lazybox-tui-installer.sh | sh
```

Both drop a `lazybox` binary on your `PATH`. Skip to [Launch](#2-launch).

:::note[Pre-1.0]
lazybox is daily-driven on macOS; Linux runs the same code paths but is less
tested. Expect sharp edges — logs land in `/tmp/lazybox.log`.
:::

### Build from source instead

Prefer to build it yourself, or hacking on lazybox? You will need **Rust 1.85+**
and a **C compiler** (lazybox bundles SQLite). On Debian/Ubuntu also install
libc++:

```sh
sudo apt install build-essential pkg-config libc++-dev libc++abi-dev
```

Then clone and build:

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
make setup   # one-shot: downloads pinned Zig 0.15.2 to ~/.cache/lazybox/zig/
```

The first compile builds the bundled SQLite and the embedded terminal, so it
takes around 30 seconds longer than later builds.

## 2. Launch

If you installed a prebuilt binary, just run:

```sh
lazybox
```

From a source checkout, use `make run` instead (it builds, then launches).

On first launch lazybox runs a short **setup wizard** to pick up your GitHub
credentials and detect installed agents and editors. When it finishes you land
on the main screen: the **sidebar inbox** on the left lists your workspaces
grouped by repo, and the larger pane on the right shows activity for the
selected row.

:::tip[Want to see the UI with zero setup?]
Run `lazybox --test` (or `cargo run -p lazybox-tui -- --test`). It boots a
throwaway tempdir repo with one seeded workspace and never touches GitHub —
the fastest way to poke at the interface before wiring up a real repo.
:::

## 3. Your win: open a workspace and spawn a session

In the sidebar:

1. Press `j` / `k` to move the selection to a workspace.
2. Press `Enter` to open it.
3. Press `c` to spawn a **Claude Code** session — or `s` for a plain **shell**
   if you'd rather not start an agent yet. `c` needs the `claude` CLI on your
   `PATH`; `s` always works.

A terminal pane opens, embedded right inside lazybox, running in that
workspace's own git worktree. Type a command; it runs in the worktree. That's
the win: lazybox gave the task an isolated worktree and a live terminal, and you
never left the inbox.

To get back to the sidebar from a terminal, press `]]` (two presses).

:::note[Don't see anything yet?]
An empty sidebar right after setup is normal, not a bug:

- The first poll can take up to ~60s (`providers.github.poll_interval`). Press
  `Shift-R` to force a refresh instead of waiting.
- Rows can be hidden by the role filter. Press `f` to widen it
  (`all → author → reviewer → assignee → mentioned → all`).
- If you simply have little GitHub activity, there may be nothing to show. You
  don't need a PR to get the win above — press `n` to spin up a fresh scratch
  workspace and open a session in it.
:::

## What next

You opened a seeded or existing workspace — now wire up a real repository:

- **[Add a repo](/docs/how-to/add-a-repo/)** so your own pull requests flow into
  the inbox.
