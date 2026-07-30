---
title: Trigger agents with @lazybox mentions
description: Let an @lazybox mention or lazybox label on a GitHub issue auto-spawn an agent on that workspace.
---

lazybox can watch for `@lazybox` mentions in your GitHub issues and
**auto-spawn an agent** on the corresponding workspace when one lands — so you
(or an allowed teammate) can kick off work by leaving a comment, without
touching the TUI.

The same works from a `lazybox:` **label**: tag one issue or a whole eligible
backlog, in a single repo or across many, and lazybox starts an agent on each.
See [Start many at once, across repos](#start-many-at-once-across-repos).

Labels have a different authorization boundary from mentions. GitHub does not
include who applied a label in the issue data lazybox polls, so
`mention.allowed_logins` applies only to `@lazybox` mentions. lazybox limits
label triggers to issues you authored or are assigned to, but anyone with
permission to label an eligible issue can trigger the agent.

## How it works

When the full GitHub sweep finds `@lazybox` in an **issue body or an issue
comment** (pull requests are not scanned), lazybox opens that issue's workspace
worktree and spawns an agent with a prompt to implement the issue. By default,
**only mentions written by the authenticated viewer** (you) trigger a spawn —
so a stranger commenting `@lazybox` on your public issue can't start an agent
on your machine.

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

When the next full GitHub sweep finds the trigger, lazybox opens the issue
workspace and maps `high` through Codex's configured
`agents.codex.models.priority` table. The selected tier's arguments can set
both the concrete model and its reasoning effort. This is an end-to-end GitHub
handoff: the issue declares the task and compute profile, and the mention
starts it without opening the TUI.

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
so leaving it on the issue does not restart the agent on every sweep.

### Start many at once, across repos

Because the trigger is just a GitHub label, you can start work in bulk. Tag a
batch of issues — from the GitHub UI, a saved search, or `gh` — with a
`lazybox:` label, in one repository or across several, and the next full GitHub
sweep opens a workspace and spawns an agent for each eligible issue you
authored or are assigned to. An eligible, triaged backlog becomes a running
fleet without opening the TUI or a terminal:

```sh
set -eu

label='lazybox:claude/M'
for repo in owner/app owner/api owner/worker; do
  # Preserve an existing label's color and description; create only if absent.
  labels=$(gh api --paginate "repos/$repo/labels?per_page=100" --jq '.[].name')
  if ! printf '%s\n' "$labels" | grep -Fxq "$label"; then
    gh label create "$label" --repo "$repo" --color 8250DF
  fi

  # Label every open "ready" issue; lazybox starts eligible ones.
  issues=$(
    gh api --paginate "repos/$repo/issues?state=open&labels=ready&per_page=100" \
      --jq '.[] | select(has("pull_request") | not) | .number'
  )
  if [ -n "$issues" ]; then
    printf '%s\n' "$issues" |
      while read -r issue; do
        gh issue edit "$issue" --repo "$repo" --add-label "$label"
      done
  fi
done
```

The paginated API calls cover every matching issue rather than stopping at a
fixed result limit. Existing label metadata is left untouched. Each label
trigger is handled once and persisted, so re-running the loop or leaving the
labels in place never respawns an agent that is already working. Every eligible
issue gets its own isolated worktree, so the agents run side by side without
colliding.

## Allow other people to trigger mentions

To let specific collaborators trigger agents with `@lazybox` mentions, list
their GitHub logins under `mention.allowed_logins` in
`~/.lazybox/config.yaml`:

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

- Mention and label triggers are scanned by the full GitHub sweep, not by the
  hot or notifications-driven incremental polls. Under normal polling, the full
  sweep runs at daemon startup and roughly every ten minutes by default, so a
  new trigger can wait about ten minutes before it starts.
- The spawned agent runs with the same tools and repository access as any other
  lazybox agent — see the [security boundaries](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md).
  Only add logins you trust to run an agent against your checkout, and only
  give trusted people permission to apply labels in repositories containing
  eligible issues.

## See also

- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/) — what
  an agent does once it's spawned in a worktree.
- [Configuration reference → `mention`](/docs/reference/configuration/) — the
  full schema.
