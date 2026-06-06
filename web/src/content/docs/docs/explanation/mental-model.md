---
title: Mental model
description: Workspace = worktree + agent, the reactive inbox, read/unread, and the isolation rationale.
---

This page explains how to think about lazybox. Once these few ideas click, the
keybindings and configuration follow naturally.

## A workspace is a worktree plus an agent session

The central unit in lazybox is the **workspace**. A workspace is two things
bound together:

1. **A git worktree** — an isolated checkout of the repository, separate from
   your main working copy and from every other workspace.
2. **An agent (or shell) session** — one embedded terminal running Claude Code,
   Codex, Cursor, or a plain shell, scoped to that worktree.

One worktree per workspace. One agent per workspace. A task — a pull request, an
issue, a piece of pre-PR work — maps to exactly one workspace.

## The reactive inbox model

Most tools make you *pull*: you refresh GitHub, scan for what changed, and
decide what to act on. lazybox inverts this. Providers (GitHub today, Linear, an
optional Slack mirror) poll upstream and **push** events onto an internal bus.
Subscribers — the TUI, the JSON API gateway — react. New comments, CI failures,
and review requests arrive on their own and surface in the sidebar.

This is the same shift email made over checking a noticeboard: the work comes to
you. The sidebar is your inbox; rows are workspaces; activity flows in.

## Read and unread

Because it behaves like an inbox, lazybox tracks **read/unread** state per
workspace, persisted in `~/.lazybox/v2/state.db` across launches. New activity
marks a row unread; opening it (or pressing `m`) marks it read. Auto-mark-read
can be undone with `z` in the activity pane. This is what lets you skim the
sidebar and immediately see *what needs attention* rather than re-reading
everything.

## Why a worktree per workspace

Isolating each task in its own git worktree buys two things:

- **No cross-contamination.** Work on a CI fix never touches the branch you have
  checked out for a different review. Each task has clean, independent state.
- **Real parallelism.** You can have several tasks in flight — each with its own
  branch, its own build artifacts, its own agent — without stashing or switching
  branches.

## Why an agent per workspace

Binding a single agent session to a single worktree keeps the agent's context
and blast radius scoped to one task. The agent works directly with `git` and
`gh` in that worktree — lazybox does not wrap those actions behind an approval
layer, so the agent has exactly the tools it would have in any checkout. When
you let an agent run autonomously (see
[Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/)), the
worktree boundary is what bounds the damage it can do.

## Where to go next

- See how these pieces are wired together in the
  [architecture explanation](/docs/explanation/architecture/).
- Put the model to work with the [How-to guides](/docs/how-to/).
