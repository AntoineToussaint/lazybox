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
  live terminal session in about five minutes.
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
brew install AntoineToussaint/lazybox/lazybox
gh auth login   # if you haven't already
lazybox
```

Then follow the [Quickstart](/docs/tutorials/quickstart/) for what you see on
first launch — it also covers the `curl | sh` and build-from-source paths.
