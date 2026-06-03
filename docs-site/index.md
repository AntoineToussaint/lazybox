# lazybox

**A reactive PR inbox in your terminal.**

Instead of refreshing GitHub in a browser, events flow to you. New comments, CI
failures, and review requests surface as they land, with read/unread tracking —
the way an email client surfaces mail. Each task becomes a *workspace*: a git
worktree plus an embedded terminal running Claude Code, Codex, Cursor, or a
plain shell.

lazybox is source-agnostic. GitHub today, Linear, and an optional Slack mirror —
all in the same inbox, with the same keys.

!!! warning "Pre-1.0, early-adopter dev mode"
    lazybox is daily-driven on macOS; Linux runs the same code paths but is less
    tested. There is no prebuilt release yet, so every install path here builds
    from source. Expect sharp edges; logs land in `/tmp/lazybox.log`.

## Who it's for

Developers who live across several open pull requests and want the work to come
to them — and who like driving an agent (Claude Code, Codex, Cursor) inside an
isolated worktree per task.

## Find your way around

The documentation is split into four sections, following the
[Diátaxis](https://diataxis.fr/) framework:

- **[Tutorials](tutorials/index.md)** — learning-oriented. Start here if you are
  new. The [Quickstart](tutorials/quickstart.md) gets you from clone to a live
  terminal session in about five minutes.
- **[How-to guides](how-to/index.md)** — task-oriented recipes: add a repo, run
  an agent per workspace, configure per-repo env and mounts, drive a remote
  daemon over SSH, mirror to Slack.
- **[Reference](reference/index.md)** — exhaustive, dry facts: the
  [CLI](reference/cli.md), every [keybinding](reference/keybindings.md), and the
  full [configuration schema](reference/configuration.md).
- **[Explanation](explanation/index.md)** — the "why": the
  [mental model](explanation/mental-model.md) and the
  [architecture](explanation/architecture.md).

## Fastest path

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox && make setup && make run
```

Then follow the [Quickstart](tutorials/quickstart.md) for what you see on first
launch.
