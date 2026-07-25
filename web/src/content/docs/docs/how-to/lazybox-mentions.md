---
title: Trigger agents with @lazybox mentions
description: Let an @lazybox mention in a GitHub issue or comment auto-spawn the default agent on that workspace.
---

lazybox can watch for `@lazybox` mentions in your GitHub issues and
**auto-spawn a Claude Code agent** on the corresponding workspace when one lands
— so you (or an allowed teammate) can kick off work by leaving a comment,
without touching the TUI.

## How it works

When the GitHub provider polls and finds `@lazybox` in an **issue body or an
issue comment** (pull requests are not scanned), lazybox opens that issue's
workspace worktree and spawns an agent with a prompt to implement the issue. By
default, **only mentions written by the authenticated viewer** (you) trigger a
spawn — so a stranger commenting `@lazybox` on your public issue can't start an
agent on your machine.

The spawned agent is currently the **Claude Code agent (`claude`)**, hardcoded —
it does not follow your `setup.default_agent` setting.

## Allow other people to trigger it

To let specific collaborators trigger agents too, list their GitHub logins under
`mention.allowed_logins` in `~/.lazybox/config.yaml`:

```yaml
mention:
  allowed_logins:
    - alice
    - bob
```

The list is authoritative:

- **Empty (or absent)** — only the authenticated viewer (you) triggers spawns.
- **Non-empty** — exactly the listed logins trigger spawns. The viewer is *not*
  added automatically, so include your own login if you want your mentions to
  keep working alongside your collaborators'.

## Notes

- This reacts to GitHub activity on the normal poll cycle (see
  `providers.github.poll_interval`), so there's a short delay between the
  comment landing and the agent starting.
- The spawned agent runs with the same tools and repository access as any other
  lazybox agent — see the [security boundaries](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md).
  Only add logins you trust to run an agent against your checkout.

## See also

- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/) — what
  an agent does once it's spawned in a worktree.
- [Configuration reference → `mention`](/docs/reference/configuration/) — the
  full schema.
