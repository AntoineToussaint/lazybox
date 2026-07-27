---
title: Documentation
description: What lazybox is, who it's for, and a map of the documentation.
---

**A reactive PR inbox in your terminal.**

Instead of refreshing GitHub in a browser, events flow to you. New comments, CI
failures, and review requests surface as they land, with read/unread tracking —
the way an email client surfaces mail. Each task becomes a *workspace*: a git
worktree plus an embedded terminal running Claude Code, Codex, Cursor, or a
plain shell.

lazybox is source-agnostic. GitHub and Linear today, with an optional Slack
mirror — all in the same inbox, with the same keys.

## Start with the core workflows

The **[Core workflows](/docs/tutorials/core-workflows/)** path follows real
tasks rather than teaching isolated controls. It shows how to:

- open an agent, shell, and editor in the correct task checkout without
  managing paths or worktrees;
- trust stable, accurate agent status and jump directly to the workspace that
  needs input or has failing CI;
- reuse recent snippets, read the per-workspace `]N` progress cue, and create a
  new snippet safely through Ask Lazybox;
- carry a live issue session into its implementation PR, then complete the
  GitHub workflow inside the TUI;
- return to the same live tmux-backed session after restarting lazybox;
- keep 10 repositories and 15 concurrent sessions controlled from one inbox;
- choose among Claude Code, Codex, Cursor, or a custom CLI while retaining a
  one-action default; and
- learn leader shortcuts through which-key and generated, remap-aware help.

Start with the [Quickstart](/docs/tutorials/quickstart/) if lazybox is not
installed yet, then take the core path before diving into reference material.

:::caution[Pre-1.0, early-adopter dev mode]
lazybox is daily-driven on macOS; Linux runs the same code paths but is less
tested. Prebuilt binaries ship via a Homebrew tap and a `curl | sh` installer
(see the [Quickstart](/docs/tutorials/quickstart/)), or build from source.
Expect sharp edges; logs land in `/tmp/lazybox.log`.
:::

## Who it's for

Developers who live across several open pull requests and want the work to come
to them — and who like driving an agent (Claude Code, Codex, Cursor) inside an
isolated worktree per task.

## Find your way around

The documentation is split into four sections, following the
[Diátaxis](https://diataxis.fr/) framework:

- **[Tutorials](/docs/tutorials/)** — learning-oriented. Start here if you are
  new. The [Quickstart](/docs/tutorials/quickstart/) gets you from clone to a
  live terminal session in about five minutes; the
  [Core workflows](/docs/tutorials/core-workflows/) then connects that session
  to the complete task, Git, agent, and GitHub lifecycle.
- **[How-to guides](/docs/how-to/)** — task-oriented recipes: add a repo, run
  an agent per workspace, orchestrate a fleet of agents, configure per-repo env
  and mounts, drive a remote daemon over SSH, mirror to Slack.
- **[Reference](/docs/reference/)** — exhaustive, dry facts: the
  [CLI](/docs/reference/cli/), every [keybinding](/docs/reference/keybindings/),
  and the full [configuration schema](/docs/reference/configuration/).
- **[Explanation](/docs/explanation/)** — the "why": the
  [mental model](/docs/explanation/mental-model/) and the
  [architecture](/docs/explanation/architecture/).

## Fastest path

```sh
brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox
gh auth login   # if you haven't already
lazybox
```

Then follow the [Quickstart](/docs/tutorials/quickstart/) for what you see on
first launch — it also covers the `curl | sh` and build-from-source paths.
