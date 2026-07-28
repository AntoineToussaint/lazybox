---
title: Start work from the issue and keep your session when it becomes a PR
description: Carry a running agent, its worktree, and its context from a GitHub issue into the pull request that closes it.
---

Start work from the issue and keep your session when it becomes a PR. When a
closing pull request appears, lazybox carries the work forward into the PR
workspace instead of making you restart the agent or copy context into a second
terminal.

:::tip[The payoff]
The agent stays live in the same working tree. Its edits, terminal output, and
conversation remain in place while the inbox moves the session under the PR
that now owns the work.
:::

## Before you start

You need:

- a GitHub repository whose issues and pull requests are visible in lazybox;
- an issue workspace in that repository; and
- a pull request that tells GitHub it closes the issue.

The reliable way to create the relationship is a closing line in the PR body:

```md
Closes #42.
```

Replace `42` with the issue number. `Fixes` and `Resolves` closing keywords work
too. When you start an issue with `w w`, lazybox's implementation brief already
asks the agent to create a PR with the closing line.

## Carry the issue session into its PR

1. Select the issue in the sidebar and press `w w`. The default agent starts in
   a dedicated worktree with the issue as its brief.
2. Let the agent open a pull request whose body closes the issue. You can also
   open the PR yourself; the relationship in GitHub is what matters.
3. Wait for the PR to sync, or press `Shift-R` to refresh.
4. If the issue has a running terminal, lazybox asks whether to join the
   issue's sessions into the PR workspace. Press `Enter` to accept.
5. Continue from the PR row. The terminal is the same live terminal, now
   attached to the PR's reviews, checks, and activity.

If the issue has no live terminal, lazybox joins it automatically without a
prompt. Recoverable session records still move to the PR; the prompt exists
only to keep running work from changing rows without your approval.

## Join later with `x j`

If you decline the automatic prompt, the issue and PR remain separate for the
current lazybox run. To carry the session over later:

1. Select the issue workspace.
2. Press `x`, then `j` (**join into PR**).

The PR must already be synced and must close the selected issue. If lazybox
says that no PR closes the issue, check the PR body for a closing keyword and
press `Shift-R`. Because `x j` is an explicit request, it performs the join
without showing the automatic confirmation again.

## What stays with you

The join preserves the working context rather than starting a replacement:

- every running agent and shell terminal, including its process, scrollback,
  composer draft, and prompt history;
- the session worktree and local edits, including uncommitted or unpushed work;
- the issue's activity and read/unread state alongside the PR's activity;
- workspace notes, recent snippets, and applicable automation settings.

If the PR already has a terminal or a worktree containing real work, lazybox
keeps it too. Only an unused, pristine PR-side stub may be retired in favor of
the issue worktree that contains the implementation.

The issue's standalone inbox row disappears, and the PR row becomes the single
home for the combined work. The linked issue, its activity, and the transferred
session remain part of that workspace.

## Related

- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/)
- [Add a repo](/docs/how-to/add-a-repo/)
- [Manage automation policies](/docs/how-to/manage-automation-policies/)
