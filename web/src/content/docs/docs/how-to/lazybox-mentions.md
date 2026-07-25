---
title: Trigger agents with @lazybox mentions
description: Let an @lazybox mention in a GitHub issue or comment auto-spawn the default agent on that workspace.
---

lazybox can watch for `@lazybox` mentions in your GitHub issues and comments and
**auto-spawn the default agent** on the corresponding workspace when one lands —
so you (or a teammate) can kick off work by leaving a comment, without touching
the TUI.

## How it works

When the GitHub provider polls and finds a new comment mentioning `@lazybox` on
a PR or issue in your inbox, lazybox opens that workspace's worktree and spawns
your [default agent](/docs/how-to/run-an-agent-per-workspace/) with the
mention's context. By default, **only mentions written by the authenticated
viewer** (you) trigger a spawn — so a stranger commenting `@lazybox` on your
public PR can't start an agent on your machine.

## Allow other people to trigger it

To let specific collaborators trigger agents too, list their GitHub logins under
`mention.allowed_logins` in `~/.lazybox/config.yaml`:

```yaml
mention:
  allowed_logins:
    - alice
    - bob
```

An empty (or absent) list keeps the default: only your own mentions count. The
authenticated viewer is always allowed and does not need to be listed.

## Notes

- This reacts to GitHub activity on the normal poll cycle (see
  `providers.github.poll_interval`), so there's a short delay between the
  comment landing and the agent starting.
- The spawned agent runs with the same tools and repository access as any other
  lazybox agent — see the [security boundaries](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md).
  Only add logins you trust to run an agent against your checkout.

## See also

- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/) — how
  the default agent is chosen and configured.
- [Configuration reference → `mention`](/docs/reference/configuration/) — the
  full schema.
