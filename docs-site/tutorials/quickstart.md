# Quickstart

In about five minutes you will build lazybox from source, launch it, and open a
workspace with a live embedded terminal. That is the whole goal of this
tutorial — one visible win. Everything else can wait.

## Prerequisites

Before you start, make sure you have:

- **Rust 1.85 or newer** (`rustc --version`).
- **A C compiler** — lazybox bundles SQLite and compiles it on the first build.
- **The GitHub CLI**, logged in: run `gh auth login` once. lazybox reads your
  token from `gh auth token`.
- **Network access to github.com** on the first build.

On Linux you also need libc++ and libc++abi. On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libc++-dev libc++abi-dev
```

## 1. Clone and build

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
make setup
```

`make setup` is a one-shot step that downloads a pinned Zig 0.15.2 toolchain to
`~/.cache/lazybox/zig/`. You only run it once.

!!! note "First build takes a moment"
    The first compile builds the bundled SQLite and the embedded terminal, so
    it takes around 30 seconds longer than later builds.

## 2. Launch

```sh
make run
```

On first launch lazybox runs a short **setup wizard** to pick up your GitHub
credentials and detect installed agents and editors. When it finishes you land
on the main screen: the **sidebar inbox** on the left lists your workspaces
grouped by repo, and the larger pane on the right shows activity for the
selected row.

!!! tip "Want to see the UI with zero setup?"
    Run `lazybox --test` (or `cargo run -p lazybox-tui -- --test`). It boots a
    throwaway tempdir repo with one seeded workspace and never touches GitHub —
    the fastest way to poke at the interface before wiring up a real repo.

## 3. Your win: open a workspace and spawn a session

In the sidebar:

1. Press `j` / `k` to move the selection to a workspace.
2. Press `Enter` to open it.
3. Press `c` to spawn a **Claude Code** session — or `s` for a plain **shell**
   if you'd rather not start an agent yet.

A terminal pane opens, embedded right inside lazybox, running in that workspace's
own git worktree. Type a command; it runs in the worktree. That's the win:
lazybox gave the task an isolated worktree and a live terminal, and you never left
the inbox.

To get back to the sidebar from a terminal, press `]]` (two presses).

## What next

You opened a seeded or existing workspace — now wire up a real repository:

- **[Add a repo](../how-to/add-a-repo.md)** so your own pull requests flow into
  the inbox.
