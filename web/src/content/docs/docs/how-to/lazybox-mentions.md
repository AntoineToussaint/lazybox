---
title: Trigger agents with @lazybox mentions
description: Let an @lazybox mention or lazybox label on a GitHub issue auto-spawn an agent on that workspace.
---

lazybox can watch for `@lazybox` mentions in your GitHub issues and
**auto-spawn an agent** on the corresponding workspace when one lands — so you
(or an allowed teammate) can kick off work by leaving a comment, without
touching the TUI.

## How it works

When the GitHub provider polls and finds `@lazybox` in an **issue body or an
issue comment** (pull requests are not scanned), lazybox opens that issue's
workspace worktree and spawns an agent with a prompt to implement the issue. By
default, **only mentions written by the authenticated viewer** (you) trigger a
spawn — so a stranger commenting `@lazybox` on your public issue can't start an
agent on your machine.

A bare `@lazybox` starts Claude Code. To choose a registered agent and an
optional model-tier alias, put them after the mention on the same line:

```text
@lazybox claude S
@lazybox codex
```

The first example starts Claude at its built-in `S` (Haiku) tier; the second
starts Codex at its configured default tier. Unknown agent ids fall back to
Claude, and an unknown model alias falls back to that agent's default.

## Let the issue choose the tier

You can choose the compute profile without putting a model alias in the
`@lazybox` directive. Add a `high`, `medium`, or `low` GitHub label, or put the
matching `@high`, `@medium`, or `@low` marker in the **issue body**:

```text
@high
@lazybox codex
```

On the next poll, lazybox opens the issue workspace and maps `high` through
Codex's configured `agents.codex.models.priority` table. The selected tier's
arguments can set both the concrete model and its reasoning effort. This is an
end-to-end GitHub handoff: the issue declares the task and compute profile, and
the mention starts it without opening the TUI.

An explicit model alias in the directive, such as `@lazybox codex S`, overrides
the issue priority. Priority labels take precedence over body markers; when
several labels or markers are present, the strongest one wins.

See [Run an agent per workspace → Let GitHub choose the model and
effort](/docs/how-to/run-an-agent-per-workspace/#let-github-choose-the-model-and-effort)
for the full tier configuration and the `w S` / `w M` / `w L` in-TUI
overrides.

## Trigger from a label

For an issue you authored or are assigned to, a
`lazybox:<agent>[/<model-alias>]` label provides the same one-shot trigger:

```text
lazybox:claude/S
lazybox:codex
```

Pull requests and issues where you are only mentioned or requested as a
reviewer are ignored. The first matching label is handled once and persisted,
so leaving it on the issue does not restart the agent on every poll.

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
  mention or label landing and the agent starting.
- The spawned agent runs with the same tools and repository access as any other
  lazybox agent — see the [security boundaries](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md).
  Only add logins you trust to run an agent against your checkout.

## See also

- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/) — what
  an agent does once it's spawned in a worktree.
- [Configuration reference → `mention`](/docs/reference/configuration/) — the
  full schema.
