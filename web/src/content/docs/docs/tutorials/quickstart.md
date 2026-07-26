---
title: Quickstart
description: Install lazybox and open your first workspace with a live terminal in about five minutes.
---

In a few minutes you will install lazybox, launch it, and open a workspace with
a live embedded terminal. That is the whole goal of this tutorial — one visible
win. Everything else can wait.

For a real GitHub inbox you need the **GitHub CLI**, logged in. Run
`gh auth login` once — lazybox reads your token from `gh auth token`. The
zero-setup `--test` mode does not need GitHub or `gh`.

## 1. Install

The fastest path is a prebuilt binary.

**Homebrew** (macOS arm64/x86_64 or Linux x86_64):

```sh
brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox
```

**Or `curl | sh`:**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AntoineToussaint/lazybox/releases/latest/download/lazybox-tui-installer.sh | sh
```

Both drop a `lazybox` binary on your `PATH`. Skip to [Launch](#2-launch).

:::note[Pre-1.0]
lazybox is daily-driven on macOS; Linux runs the same code paths but is less
tested. Expect sharp edges — logs land in `/tmp/lazybox.log`.
:::

### Build from source instead

Prefer to build it yourself, or hacking on lazybox? You will need **Rust 1.88+**
and a **C compiler** (lazybox bundles SQLite). On Debian/Ubuntu also install
libc++:

```sh
sudo apt install build-essential pkg-config libc++-dev libc++abi-dev
```

Then clone and build:

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
make setup     # one online preparation of Zig, Ghostty, and Cargo caches
make release   # optional optimized build; strictly offline after setup
```

`make setup` verifies the pinned Zig archive, caches the pinned Ghostty source,
fetches the locked Cargo graph, and prebuilds the native terminal dependency.
It is the only step that requires network access. Later `make release` runs
Cargo with `--offline --locked`, including after `cargo clean` or from another
worktree sharing the same cache.

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

![lazybox inbox with repository workspaces in the sidebar, activity in the upper-right pane, and embedded agent terminals below](/demo/lazybox.png)

:::tip[Want to see the UI with zero setup?]
Run `lazybox --test` (or `cargo run -p lazybox-tui-boot -- --test`). It boots a
throwaway tempdir repo with one seeded workspace and never touches GitHub —
the fastest way to poke at the interface before wiring up a real repo.
:::

## 3. Your win: open a workspace and spawn a session

In the sidebar:

1. Press `j` / `k` to move the selection to a workspace.
2. Press `Enter` to open it.
3. Press `w w` to put your **default agent** to work — or `s` for a plain
   **shell** if you'd rather not start an agent yet. `w w` needs the agent's CLI
   (e.g. `claude`) on your `PATH`; `s` always works. To pick a specific agent,
   press `a` for the agent menu: `a c` Claude Code, `a x` Codex, `a u` Cursor.

A terminal pane opens, embedded right inside lazybox, running in that
workspace's own git worktree. Type a command; it runs in the worktree. That's
the win: lazybox gave the task an isolated worktree and a live terminal, and you
never left the inbox.

To get back to the sidebar from a terminal, press `]]` (two presses) then `q`
— `]]` opens a small command menu, and `]]q` exits to the sidebar.

:::note[Don't see anything yet?]
An empty sidebar right after setup is normal, not a bug:

- The first poll can take up to ~60s (`providers.github.poll_interval`). Press
  `Shift-R` to force a refresh instead of waiting.
- Rows can be hidden by active filters. Press `f` to open the filter menu and
  clear any toggled predicates.
- If you simply have little GitHub activity, there may be nothing to show. You
  don't need a PR to get the win above — press `x n` to spin up a fresh scratch
  workspace and open a session in it.
:::

## What next

You opened a seeded or existing workspace — now wire up a real repository:

- **[Add a repo](/docs/how-to/add-a-repo/)** so your own pull requests flow into
  the inbox.
