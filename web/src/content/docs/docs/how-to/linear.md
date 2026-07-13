---
title: Connect Linear
description: Bring your Linear issues into the lazybox inbox alongside GitHub.
---

Goal: surface your Linear issues in the inbox, next to GitHub PRs and issues —
same rows, same read/unread tracking, same worktree-per-task workflow.

## Prerequisites

- lazybox is running (see the [Quickstart](/docs/tutorials/quickstart/)).
- A Linear account and a **personal API key** — create one under
  Linear → Settings → Security & access → **Personal API keys**.

## 1. Set the API key

lazybox reads the key from the `LINEAR_API_KEY` environment variable:

```sh
export LINEAR_API_KEY=lin_api_…
```

Put it in your shell profile so it's present whenever lazybox starts. There is
no YAML field for the key — the environment variable is the only source.

## 2. Enable the provider

The first-run setup wizard detects a working `LINEAR_API_KEY` and offers
Linear as a provider. On an existing install, press `,` (Settings) and enable
Linear there. Either way the choice persists to `setup.providers` in
`~/.lazybox/config.yaml`:

```yaml
setup:
  providers: [github, linear]
```

Restart lazybox (or press `Shift-R` to refresh) and the Linear poller starts.

## What appears in the inbox

- Issues you are **assigned to or created**, fetched via Linear's GraphQL API.
  Issues in the `completed` / `canceled` states are filtered out server-side.
- Each issue is a normal inbox row (marked with the `◆` Linear glyph, or `l`
  with `display.ascii_glyphs: true`), grouped by team, with the same
  read/unread tracking as GitHub rows.
- Everything workspace-shaped works the same: `Enter` opens it, `s` opens a
  shell, `w` puts your default agent on it — lazybox cuts a branch and an
  isolated worktree for the ticket just as it does for a GitHub issue.

## Role filtering

Which of your Linear issues show up is controlled by the per-provider filter
keys (the wizard's filter step, or `setup.filters.linear` in YAML):

| Key | Shows |
| --- | --- |
| `role.assignee` | Issues assigned to you (the default) |
| `role.author` | Issues you created |
| `role.mentioned` | Issues where you're mentioned |

Linear has no reviewer concept, so there is no `role.reviewer` key. In the
sidebar, `f` cycles the role filter across whatever is enabled.

## Limitations

- The key must be in the daemon's environment at startup; rotating it means
  restarting lazybox.
- Some mutations that exist for GitHub are not implemented for Linear (for
  example, `g m` merge has no Linear equivalent — moving an issue to Done is
  done in Linear itself).

## Related

- The [CLI reference](/docs/reference/cli/#environment-variables) for every
  environment variable.
- The [configuration reference](/docs/reference/configuration/#setup) for the
  `setup` block.
