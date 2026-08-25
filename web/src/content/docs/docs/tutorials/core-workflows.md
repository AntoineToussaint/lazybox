---
title: Core workflows
description: Follow lazybox's complete task, Git, agent, and GitHub workflows from first action to cleanup.
---

lazybox is most useful as one continuous loop: a task arrives, every tool opens
in its correct checkout, an agent works with normal Git, and the resulting
GitHub state flows back to the same workspace. This page shows that loop and
the workflows that make it manageable across many repositories.

If this is your first launch, complete the
[Quickstart](/docs/tutorials/quickstart/) first. Return here once you can see the
inbox.

## Choose a workflow

1. [Always open in the right folder](#always-open-in-the-right-folder)
2. [Follow the workspace that needs attention](#follow-the-workspace-that-needs-attention)
3. [Reuse snippets and see progress](#reuse-snippets-and-see-progress)
4. [Start on an issue and continue on its PR](#start-on-an-issue-and-continue-on-its-pr)
5. [Restart without stopping sessions](#restart-without-stopping-sessions)
6. [Complete GitHub work inside lazybox](#complete-github-work-inside-lazybox)
7. [Control a multi-repository workload](#control-a-multi-repository-workload)
8. [Turn Ask Lazybox guidance into a snippet](#turn-ask-lazybox-guidance-into-a-snippet)
9. [Choose agents without losing the fast path](#choose-agents-without-losing-the-fast-path)
10. [Learn shortcuts as you use them](#learn-shortcuts-as-you-use-them)

## Always open in the right folder

The primary benefit of task worktrees is that you do not manage them. Select a
GitHub issue, PR, or local workspace in any repository, then:

1. Press `w w` to put the default agent to work.
2. Return to the sidebar with `]]q`, then press `s` for a shell.
3. Return again and press `e` for your editor.
4. Run `pwd` in the shell and inspect the folder opened by the editor. Both
   point at the same task checkout the agent is using.

lazybox creates the checkout when the first session needs it, scopes every
action to it, reuses it for that workspace, and handles cleanup with the
workspace lifecycle. You never need to create, locate, switch, or clean up the
task worktree by hand.

Inside it, nothing is proprietary: the agent and shell use normal `git` and
`gh`. Inspect changes, commit, push, and open the PR exactly as you would from
any checkout. GitHub checks, reviews, comments, and merge state then return to
the workspace through the reactive inbox.

For the underlying checkout model, see the
[mental model](/docs/explanation/mental-model/#a-workspace-is-a-worktree-plus-an-agent-session).
For agent prompts, model tiers, and terminal layouts, see
[Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/).

## Follow the workspace that needs attention

Run several agents, then leave their terminals. lazybox's **Working**,
**Input Needed**, and **Done** detector is stable and accurate enough to be the
primary attention signal:

1. Press `!` to jump straight to the next agent waiting for input.
2. Answer it, then return to the inbox with `]]q`.
3. Press `Shift-F` to jump to a PR with failing CI and use `w w` to start the
   appropriate fix.
4. Use unread, review, conflict, and sync indicators to choose the next
   workspace without reopening every terminal or refreshing GitHub.

The states are part of the workspace, alongside provider activity and CI, so
the question is always “which task needs me?” rather than “which terminal might
have stopped?”

The [keybindings reference](/docs/reference/keybindings/#global) lists the
attention jumps, and
[desktop notifications](/docs/how-to/desktop-notifications/) can surface the
same transitions while lazybox is unfocused.

## Reuse snippets and see progress

Snippets retain both global recency and workspace-specific progress:

1. In a terminal, type `]]srev` to send the built-in review snippet.
2. Switch to another workspace and send `]]stest`.
3. Return to the first workspace. Its sidebar row shows `]1`: one distinct
   snippet workflow has been applied there.
4. Send another distinct snippet and the badge becomes `]2`.
5. Open `]]s`. Sent snippets are grouped under **Recent**, with the latest
   selected. Press `Enter` to repeat it immediately.
6. Restart lazybox and open the picker again. Recent order persists in the
   state database.

The `]N` badge counts distinct snippet keys for that workspace, not total sends,
so repeating `rev` stays a progress cue rather than an invocation counter.
Snippet state follows the workspace when sessions are adopted or an issue is
joined into its PR.

Continue with [Use snippets](/docs/how-to/use-snippets/) for filtering,
broadcast, file precedence, and creating your own prompts.

## Start on an issue and continue on its PR

You do not need to wait for a pull request before starting implementation:

1. Select a GitHub issue and press `w w`. The agent starts in the issue's
   managed checkout.
2. Let the agent commit, push, and open a PR with `gh pr create`, including a
   closing reference such as `Closes #184`.
3. When lazybox discovers the PR, confirm the offered join if the issue has
   live terminals. lazybox can also identify the relationship from the task
   branch when the closing reference is not yet available.
4. Continue in the PR workspace. The live terminal and backend process have not
   restarted; the task checkout, agent context, prompts, snippets, notes, and
   activity move with it.
5. If both rows remain visible and the PR is already synced, select the issue
   and press `x j` to join it manually.

The result is one PR row with the work already in progress—no duplicate
terminal and no manual context handoff. The exact control is also listed under
the [workspace leader](/docs/reference/keybindings/#x--workspace).

## Restart without stopping sessions

Production lazybox automatically uses tmux when **tmux 3.3 or newer** is on
`PATH`.

1. Start a long-running agent or shell task.
2. Note a distinctive line in its output or scrollback.
3. Close the lazybox UI/server process while the command is still running.
4. Start lazybox again.
5. Open the workspace. lazybox reattaches to the same live tmux session and
   reconstructs its screen and retained scrollback; the distinctive line and
   continued output prove this is the same session.

If supported tmux is missing, lazybox automatically uses the raw-PTY backend.
Embedded terminals still work, and their last scrollback can be restored, but
the child process itself does not survive a lazybox process restart. Install or
upgrade tmux to enable live-session continuity.

For daemon and reconnect details, see the
[architecture explanation](/docs/explanation/architecture/).

## Complete GitHub work inside lazybox

Start from an unread GitHub event and carry it to completion:

1. Open the workspace to read the full issue or PR description and activity.
2. Move through comments and checks; press `r` to reply without leaving the
   TUI.
3. Press the GitHub leader `g`, then use `g r` for reviewers, `g a` for
   assignees, or `g l` for labels.
4. Inspect CI, review, and conflict state in the workspace. Use `g g` to arm
   lazybox's merge-on-green behavior or `g m` to merge an eligible PR now.
   `g s` runs a targeted sync of just this workspace — cheap when you're waiting
   on one PR's CI. On a **repo-scoped** workspace (no single PR/issue), `g s`
   instead **discovers** the repo's open issues and PRs so you can pull them in
   without a full `Shift-R` sweep.
5. On an issue, `x c` closes it upstream after confirmation. When the work is
   finished, `x x` archives the workspace and cleans up its sessions. To do both
   at once — close/delete the issue or PR upstream **and** archive the workspace
   (killing its sessions) — use `x k` (close & kill), a single confirmed step for
   ending a finished line of work.

The browser remains available through `g o`, but it is an escape hatch rather
than a required step. See the
[full GitHub and workspace leaders](/docs/reference/keybindings/#leader-menus)
for exact controls.

## Control a multi-repository workload

A representative lazybox workload can have **10 repositories and 15 live
sessions**. Keep it controlled by navigating outcomes instead of terminals:

1. Use the sidebar's repository groups to keep each task, agent state, GitHub
   activity, and terminal together. Group repos one tier higher into **Spaces**
   with `x m` (move the cursor's source into a named Space — repos across owners
   collect under one header); the assignment and its collapse state persist.
2. Press `` ` `` for the fuzzy workspace picker across all repositories.
3. Press `!` for the agent requesting input or `Shift-F` for failing CI. Press
   `f` and toggle the **working** predicate to see only workspaces whose agent is
   live-working right now.
4. From inside a terminal, use `` ]]` `` for the same cross-repository picker.
5. Multi-select rows with `v`, then use `Shift-B` to broadcast one snippet or
   instruction across the selected live sessions.
6. Open the **Hopper** with `Shift-H` (or `]]H` from inside a terminal) — a
   persistent scratch list where each line is its own workspace, so a stream of
   "do this next" items each become a place you can jump to and run an agent.
7. In focus mode, `]]v` cycles the layout Single → SplitV → SplitH → 2×2 Grid
   over your starred workspaces, so several live agents stay on screen at once;
   the choice persists as `ui.focus_layout`.

Each row's pinned `you ▸` recap shows the last prompt sent to that workspace's
agent, now with its **age**, so you can tell at a glance how long ago you last
directed each session.

You can move among many repositories without remembering paths because every
jump lands in a workspace that already owns its checkout and terminals.

The [multi-agent orchestration guide](/docs/how-to/orchestrate-multiple-agents/)
covers broadcast, bulk branch updates, and agent-to-agent handoff.

## Turn Ask Lazybox guidance into a snippet

Ask Lazybox is both live help and a small, deliberately bounded action surface:
Conversational answers and proposed actions require Claude Code or Codex to be
enabled; live effective-keybinding search remains available with any agent
setup.

1. Press `?` and ask, “How should I review an API change?”
2. Follow up with, “Create that as a snippet named `review-api`.”
3. Inspect the proposed key, category, description, and full body in the
   confirm-with-preview.
4. Confirm it. lazybox validates the proposal and writes the global snippet
   file itself.
5. In an agent terminal, type `]]sreview-api` immediately. The catalog
   hot-reloads; no restart is required.
6. After use, the snippet appears in **Recent** and contributes to that
   workspace's `]N` progress badge.

The assistant cannot write arbitrary files or run arbitrary in-app actions.
Actions and editable settings are allowlisted, values are validated against
live state, and nothing is applied before your confirmation. The agent only
proposes structured intent; lazybox owns the filesystem mutation.

See [Use snippets](/docs/how-to/use-snippets/#create-or-improve-a-snippet-with-ask-lazybox)
for more examples.

## Choose agents without losing the fast path

lazybox ships first-class integrations for **Claude Code, Codex, and Cursor**,
plus a generic YAML integration for other agent CLIs:

1. In Settings, enable the agents installed on your system and choose a
   preferred default.
2. Select a task and press `w w`. The default agent starts immediately with
   the contextual task prompt and managed worktree.
3. On a task that needs another tool, press `a` for the agent which-key menu:
   `a c` selects Claude Code, `a x` Codex, and `a u` Cursor.
4. Use `w S`, `w M`, or `w L` to choose the target agent's small, medium, or
   large configured model tier.

Default means convenience, not lock-in. Built-in and generic agents all
participate in the same worktree, status detection, tmux persistence, model
tier, snippet, and GitHub workflow.

See [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/) for
terminal behavior and the
[`agents.<id>` configuration](/docs/reference/configuration/#agentsid) for
custom CLIs and model tiers.

## Learn shortcuts as you use them

The shortcut model is designed around progressive discovery:

1. Focus a workspace and press `a`. The which-key popup shows the enabled
   agents and their valid continuation keys.
2. Cancel or choose one, then press `g` to see GitHub operations as another
   labeled leader group.
3. Focus a terminal and press `]]`. This separate, non-timed leader exposes
   lazybox commands while ordinary keys continue to reach the embedded agent
   or shell.
4. Press `?` to search the effective keymap, then press `?` again at the empty
   prompt for the generated shortcut index.
5. Choose the `default` or `vim` preset, or add alternatives under
   `ui.action_keys`. Footer hints, context menus, Ask Lazybox, and help all
   update from the effective action catalog rather than separate key tables.

You can therefore learn only the next key needed for the current context.
Continue with the [keybindings reference](/docs/reference/keybindings/) for the
full action catalog and
[configuration reference](/docs/reference/configuration/#action_keys--remapping-keys)
for presets and remapping syntax.
